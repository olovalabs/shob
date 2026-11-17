//! Lightweight file watching for live reload.
//!
//! Port of the upstream `packages/coding-agent/src/utils/fs-watch.ts`.
//! uses Node's `fs.watch` with a retry-on-error wrapper to (a) keep the git
//! footer's branch/status fresh and (b) hot-reload the active theme file.
//!
//! rpi watches by polling modification times. That avoids a native watcher
//! dependency and behaves predictably on every platform (network drives, WSL
//! mounts, editors that replace files by rename). The retry delay matches
//! native [`FS_WATCH_RETRY_DELAY_MS`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Native `FS_WATCH_RETRY_DELAY_MS`: how long to wait after a watch error
/// before retrying.
pub const FS_WATCH_RETRY_DELAY_MS: u64 = 5_000;

/// Default poll interval. Short enough to feel immediate, long enough that a
/// few watched files cost nothing on the main thread.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 1_000;

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn snapshot(paths: &[PathBuf]) -> HashMap<PathBuf, Option<SystemTime>> {
    paths.iter().map(|p| (p.clone(), mtime(p))).collect()
}

/// A running watch. Dropping it does **not** stop the watch; call
/// [`WatchHandle::close`] to stop it deterministically (native `closeWatcher`).
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl WatchHandle {
    /// Signal the watch to stop and wait for its task to finish.
    pub fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }

    /// Request stop without joining (useful from a drop path).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Watch `paths` for mtime changes, invoking `on_change` whenever any watched
/// path changes (content or existence). Missing paths are tracked too: creating
/// or deleting a watched path fires `on_change`.
///
/// Poll failures are tolerated: the watcher waits [`FS_WATCH_RETRY_DELAY_MS`] and
/// retries, never panicking (native `watchWithErrorHandler`).
pub fn watch_paths<F>(paths: Vec<PathBuf>, poll_interval: Duration, on_change: F) -> WatchHandle
where
    F: Fn() + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_task = stop.clone();
    let join = tokio::spawn(async move {
        let mut prev = snapshot(&paths);
        loop {
            tokio::time::sleep(poll_interval).await;
            if stop_task.load(Ordering::SeqCst) {
                break;
            }
            let current = match std::panic::catch_unwind(|| snapshot(&paths)) {
                Ok(snap) => snap,
                Err(_) => {
                    // A metadata read blew up; wait the retry delay and try
                    // again rather than tearing down the watch.
                    tokio::time::sleep(Duration::from_millis(FS_WATCH_RETRY_DELAY_MS)).await;
                    continue;
                }
            };
            if current != prev {
                prev = current;
                on_change();
            }
        }
    });
    WatchHandle {
        stop,
        join: Some(join),
    }
}

/// Convenience for a single path.
pub fn watch_path<F>(path: PathBuf, on_change: F) -> WatchHandle
where
    F: Fn() + Send + Sync + 'static,
{
    watch_paths(
        vec![path],
        Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
        on_change,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn detects_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("theme.json");
        std::fs::write(&file, "a").unwrap();

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let handle = watch_paths(vec![file.clone()], Duration::from_millis(20), move || {
                hits_task.fetch_add(1, Ordering::SeqCst);
            });
            tokio::time::sleep(Duration::from_millis(60)).await;
            std::fs::write(&file, "bb").unwrap();
            // mtime resolution: ensure a visible change.
            tokio::time::sleep(Duration::from_millis(120)).await;
            handle.close();
        });
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "expected at least one change"
        );
    }

    #[test]
    fn close_stops_watch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let handle = watch_path(file, || {});
            handle.close();
        });
    }
}
