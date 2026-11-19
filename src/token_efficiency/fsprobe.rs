//! The file access the cache is allowed to perform, behind a trait.
//!
//! Every cache decision is "compare the bytes on disk to the bytes I last saw".
//! Routing that through a trait keeps the tests hermetic — they can rewrite a
//! file without touching a real one — and keeps the one place that does I/O
//! visible: the hooks must never block the GPUI thread, and this trait is the
//! only thing they call.

use std::path::{Path, PathBuf};

/// What a stat tells us. `mtime_ms` is milliseconds since the Unix epoch, the
/// same unit as `tools::FileInfo::mtime_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub len: u64,
    pub mtime_ms: i64,
}

/// Metadata plus bytes, read once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    pub meta: FileMeta,
    pub bytes: Vec<u8>,
}

pub trait FileProbe: Send + Sync {
    /// Metadata for `path`, or `None` when it does not exist or is not a file.
    fn metadata(&self, path: &Path) -> Option<FileMeta>;
    /// The file's bytes, or `None` when it cannot be read.
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
    /// Resolve symlinks so one file has one identity. Falls back to the input
    /// when the path cannot be canonicalized.
    fn canonicalize(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Read bytes and metadata together so the stamp describes one consistent
    /// version of the file rather than two reads that could straddle a write.
    fn snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let bytes = self.read(path)?;
        let meta = self.metadata(path)?;
        Some(FileSnapshot { meta, bytes })
    }
}

/// The real filesystem.
pub struct OsFileProbe;

impl FileProbe for OsFileProbe {
    fn metadata(&self, path: &Path) -> Option<FileMeta> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(FileMeta {
            len: metadata.len(),
            mtime_ms: mtime_ms(&metadata),
        })
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }
}

fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Test double. `#[cfg(test)]` would be tidier, but the replay benchmark
/// (`bench.rs`) is shipped code that drives the real cache through this probe, so
/// building it unconditionally keeps one implementation of the fake rather than a
/// second copy in `bench`.
pub(crate) mod testing {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory filesystem: paths are exact strings, `mtime_ms` is settable
    /// independently of content so a test can reproduce "wrote the same number
    /// of bytes within one millisecond".
    #[derive(Default)]
    pub struct FakeFs {
        files: Mutex<BTreeMap<PathBuf, (Vec<u8>, i64)>>,
        canonical: Mutex<BTreeMap<PathBuf, PathBuf>>,
        clock: Mutex<i64>,
    }

    impl FakeFs {
        pub fn new() -> Self {
            Self {
                files: Mutex::new(BTreeMap::new()),
                canonical: Mutex::new(BTreeMap::new()),
                clock: Mutex::new(1_700_000_000_000),
            }
        }

        /// Write `content` and advance the clock by one millisecond.
        pub fn write(&self, path: &str, content: &str) {
            let mut clock = self.clock.lock().unwrap();
            *clock += 1;
            let now = *clock;
            self.files
                .lock()
                .unwrap()
                .insert(PathBuf::from(path), (content.as_bytes().to_vec(), now));
        }

        /// Write `content` without advancing the clock, so `mtime` and length
        /// are identical to the previous version. The adversarial case.
        pub fn write_same_mtime(&self, path: &str, content: &str) {
            let mtime = self.mtime_of(path);
            self.files
                .lock()
                .unwrap()
                .insert(PathBuf::from(path), (content.as_bytes().to_vec(), mtime));
        }

        /// Advance `mtime` without changing content: `touch`, or a `git checkout`.
        pub fn touch(&self, path: &str) {
            let mut clock = self.clock.lock().unwrap();
            *clock += 1000;
            let now = *clock;
            if let Some(entry) = self.files.lock().unwrap().get_mut(&PathBuf::from(path)) {
                entry.1 = now;
            }
        }

        pub fn remove(&self, path: &str) {
            self.files.lock().unwrap().remove(&PathBuf::from(path));
        }

        /// Make `alias` resolve to `target` under `canonicalize`.
        pub fn alias(&self, alias: &str, target: &str) {
            self.canonical
                .lock()
                .unwrap()
                .insert(PathBuf::from(alias), PathBuf::from(target));
        }

        /// Read `path` directly, without going through the `FileProbe` trait, so
        /// callers that hold a concrete `FakeFs` need no trait import.
        pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .map(|(bytes, _)| bytes.clone())
        }

        pub fn mtime_of(&self, path: &str) -> i64 {
            self.files
                .lock()
                .unwrap()
                .get(&PathBuf::from(path))
                .map(|(_, mtime)| *mtime)
                .unwrap_or(0)
        }
    }

    impl FileProbe for FakeFs {
        fn metadata(&self, path: &Path) -> Option<FileMeta> {
            self.files.lock().unwrap().get(path).map(|(bytes, mtime)| {
                FileMeta {
                    len: bytes.len() as u64,
                    mtime_ms: *mtime,
                }
            })
        }

        fn canonicalize(&self, path: &Path) -> PathBuf {
            self.canonical
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.to_path_buf())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeFs;
    use super::*;

    #[test]
    fn probe_reads_metadata_and_bytes() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "hello");
        let probed = OsFileProbe;
        // The in-memory probe is what the cache uses; the OS probe is exercised
        // below against a real temp file.
        assert_eq!(probed.metadata(Path::new("/nonexistent-xyz")), None);
        assert_eq!(fs.metadata(Path::new("/a.rs")).map(|m| m.len), Some(5));
        assert_eq!(fs.read(Path::new("/a.rs")).as_deref(), Some(&b"hello"[..]));

        let snapshot = fs.snapshot(Path::new("/a.rs")).expect("snapshot");
        assert_eq!(snapshot.bytes, b"hello");
        assert_eq!(snapshot.meta.len, 5);
    }

    #[test]
    fn os_probe_reports_len_and_mtime_for_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "abcdef").expect("write");

        let probed = OsFileProbe;
        let meta = probed.metadata(&path).expect("metadata");
        assert_eq!(meta.len, 6);
        assert!(meta.mtime_ms > 0, "mtime must be populated");
        assert_eq!(probed.read(&path).as_deref(), Some(&b"abcdef"[..]));

        // Directories are not files.
        assert_eq!(probed.metadata(dir.path()), None);
    }

    #[test]
    fn absent_file_has_no_metadata_or_bytes() {
        let fs = FakeFs::new();
        assert!(fs.metadata(Path::new("/gone")).is_none());
        assert!(fs.read(Path::new("/gone")).is_none());
        assert!(fs.snapshot(Path::new("/gone")).is_none());
    }

    #[test]
    fn fake_fs_can_hold_mtime_constant() {
        let fs = FakeFs::new();
        fs.write("/a", "aaaa");
        let first = fs.mtime_of("/a");
        fs.write_same_mtime("/a", "bbbb");
        assert_eq!(fs.mtime_of("/a"), first);
        assert_eq!(fs.read(Path::new("/a")).as_deref(), Some(&b"bbbb"[..]));
    }
}
