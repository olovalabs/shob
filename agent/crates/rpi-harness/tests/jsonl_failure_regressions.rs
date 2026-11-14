#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
    use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
    use rpi_harness::session::types::*;
    use rpi_tools::env::AppendFile;
    use rpi_tools::{FileContent, FileError, FileInfo, FileSystem};
    use std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    fn entry(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(id, 0).into(),
                terminate: None,
            },
        }
    }
    fn base(id: &str) -> RecordBase {
        RecordBase {
            id: id.into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        }
    }
    fn frame(id: &str) -> LaneRecord {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: base(id),
            run_id: "run".into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        })
    }
    fn abort(id: &str) -> LaneRecord {
        LaneRecord::AbortRequested(AbortRequestedRecord {
            base: base(id),
            run_id: "run".into(),
        })
    }
    fn header() -> JsonlV4Header {
        JsonlV4Header::new("independent".into(), 1, "/".into(), None, None, None)
    }
    #[derive(Default)]
    struct Gate {
        fail_prefix: std::sync::atomic::AtomicUsize,
        armed: AtomicBool,
        fail_after_write: AtomicBool,
        truncate_fail: AtomicBool,
        corrupt_prefix: AtomicBool,
        fail_stage: AtomicBool,
        rename_mode: std::sync::atomic::AtomicUsize,
        block_rename: AtomicBool,
        block_truncate: AtomicBool,
        entered: Notify,
        release: Notify,
    }
    struct GatedFs {
        inner: Arc<dyn FileSystem>,
        gate: Arc<Gate>,
    }
    struct GatedAppend {
        inner: Arc<dyn FileSystem>,
        path: String,
        gate: Arc<Gate>,
    }
    #[async_trait]
    impl AppendFile for GatedAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            if self.gate.corrupt_prefix.swap(false, Ordering::SeqCst) {
                let mut prior = self.inner.read_binary_file(&self.path, None).await?;
                prior[0] = b'[';
                self.inner
                    .write_file(&self.path, FileContent::Bytes(prior), None)
                    .await?;
                self.inner
                    .append_file(&self.path, FileContent::Bytes(bytes[..7].to_vec()), None)
                    .await?;
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "foreign prefix plus uncertain append",
                ));
            }
            let mode = self.gate.fail_prefix.swap(0, Ordering::SeqCst);
            if mode > 0 {
                let boundary = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
                let end = match mode {
                    1 => boundary,
                    2 => boundary + 7,
                    _ => bytes.len(),
                };
                self.inner
                    .append_file(&self.path, FileContent::Bytes(bytes[..end].to_vec()), None)
                    .await?;
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled batch write failure",
                ));
            }
            self.inner
                .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
                .await?;
            if self.gate.fail_after_write.swap(false, Ordering::SeqCst) {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled failure after bytes became visible",
                ));
            }
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            Ok(())
        }
    }
    include!("support/progress_fault_cases.rs");
    include!("support/progress_retirement_faults.rs");

    #[async_trait]
    impl FileSystem for GatedFs {
        fn cwd(&self) -> &Path {
            self.inner.cwd()
        }
        async fn absolute_path(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.absolute_path(p, c).await
        }
        async fn join_path(
            &self,
            p: &[&str],
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.join_path(p, c).await
        }
        async fn read_text_file(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<String, FileError> {
            self.inner.read_text_file(p, c).await
        }
        async fn read_text_lines(
            &self,
            p: &str,
            n: Option<usize>,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<String>, FileError> {
            self.inner.read_text_lines(p, n, c).await
        }
        async fn read_binary_file(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<u8>, FileError> {
            self.inner.read_binary_file(p, c).await
        }
        async fn write_file(
            &self,
            p: &str,
            b: FileContent,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            if self.gate.fail_stage.swap(false, Ordering::SeqCst)
                && p.ends_with(".progress.jsonl.tmp")
            {
                self.inner
                    .write_file(p, FileContent::Text("partial staging".into()), c)
                    .await?;
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled checkpoint staging failure",
                ));
            }
            self.inner.write_file(p, b, c).await
        }
        async fn append_file(
            &self,
            p: &str,
            b: FileContent,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.append_file(p, b, c).await
        }
        async fn truncate_file(
            &self,
            p: &str,
            n: u64,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            if self.gate.truncate_fail.load(Ordering::SeqCst) {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled truncate failure",
                ));
            }
            if self.gate.block_truncate.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            self.inner.truncate_file(p, n, c).await
        }
        async fn open_append_file(
            &self,
            p: &str,
        ) -> Result<Option<Box<dyn AppendFile>>, FileError> {
            Ok(Some(Box::new(GatedAppend {
                inner: self.inner.clone(),
                path: p.into(),
                gate: self.gate.clone(),
            })))
        }
        async fn rename_file(
            &self,
            p: &str,
            q: &str,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            let mode = self.gate.rename_mode.swap(0, Ordering::SeqCst);
            if self.gate.block_rename.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            if mode == 1 {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled failure before rename",
                ));
            }
            self.inner.rename_file(p, q, c).await?;
            if mode == 2 {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled failure after rename",
                ));
            }
            Ok(())
        }
        async fn file_info(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<FileInfo, FileError> {
            self.inner.file_info(p, c).await
        }
        async fn list_dir(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<FileInfo>, FileError> {
            self.inner.list_dir(p, c).await
        }
        async fn canonical_path(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.canonical_path(p, c).await
        }
        async fn exists(&self, p: &str, c: Option<&CancellationToken>) -> Result<bool, FileError> {
            self.inner.exists(p, c).await
        }
        async fn create_dir(
            &self,
            p: &str,
            b: bool,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.create_dir(p, b, c).await
        }
        async fn remove(
            &self,
            p: &str,
            b: bool,
            f: bool,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.remove(p, b, f, c).await
        }
        async fn create_temp_dir(
            &self,
            p: Option<&str>,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.create_temp_dir(p, c).await
        }
        async fn create_temp_file(
            &self,
            p: &str,
            s: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.create_temp_file(p, s, c).await
        }
        async fn cleanup(&self) {
            self.inner.cleanup().await;
        }
    }

    #[tokio::test]
    async fn jsonl_failed_truncate_and_foreign_suffix_preserve_tail_and_block_writes() {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "/test.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        storage.append_entry(entry("seed"), "main").await.unwrap();
        let before = fs.read_binary_file("/test.jsonl", None).await.unwrap();
        gate.fail_prefix.store(2, Ordering::SeqCst);
        gate.truncate_fail.store(true, Ordering::SeqCst);
        assert!(storage
            .append_records(vec![frame("f1"), frame("f2")])
            .await
            .is_err());
        let after = fs.read_binary_file("/test.jsonl", None).await.unwrap();
        assert!(after.starts_with(&before));
        assert!(after.len() > before.len());
        assert!(storage
            .append_entry(entry("blocked"), "main")
            .await
            .is_err());
        let reopened = JsonlSessionStorage::load(fs.clone(), "/test.jsonl", clock, ids)
            .await
            .expect("reopen may repair the torn tail");
        assert!(reopened
            .append_entry(entry("after-reopen"), "main")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn failed_multiline_batches_roll_back_at_line_boundaries_and_midline() {
        for mode in [1, 2, 3] {
            for shape in ["frames", "mixed", "pair", "triple"] {
                let gate = Arc::new(Gate::default());
                let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
                    inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
                    gate: gate.clone(),
                });
                let clock = Arc::new(FakeClock::new());
                let ids = Arc::new(CounterIdGenerator::new());
                let s = JsonlSessionStorage::create(
                    fs.clone(),
                    "/test.jsonl",
                    header(),
                    clock.clone(),
                    ids.clone(),
                )
                .await
                .unwrap();
                s.append_entry(entry("seed"), "main").await.unwrap();
                let before = fs.read_binary_file("/test.jsonl", None).await.unwrap();
                gate.fail_prefix.store(mode, Ordering::SeqCst);
                let result = match shape {
                    "frames" => s
                        .append_records(vec![frame("f1"), frame("f2")])
                        .await
                        .map(|_| ()),
                    "mixed" => s
                        .append_records(vec![frame("f1"), abort("a1")])
                        .await
                        .map(|_| ()),
                    "pair" => s
                        .append_record_and_entry(abort("before"), entry("failed"), "main")
                        .await
                        .map(|_| ()),
                    _ => s
                        .append_record_entry_and_record(
                            abort("before"),
                            entry("failed"),
                            "main",
                            abort("after"),
                        )
                        .await
                        .map(|_| ()),
                };
                assert!(result.is_err(), "{shape} mode {mode}");
                assert_eq!(
                    fs.read_binary_file("/test.jsonl", None).await.unwrap(),
                    before,
                    "failed batch prefix remained: shape={shape} mode={mode}"
                );
                let ack = s.append_entry(entry("accepted"), "main").await.unwrap();
                assert_eq!(ack.seq(), 2);
                let loaded = JsonlSessionStorage::load(fs, "/test.jsonl", clock, ids)
                    .await
                    .unwrap();
                assert_eq!(
                    s.get_log(&LogOptions::default()).await.unwrap(),
                    loaded.get_log(&LogOptions::default()).await.unwrap()
                );
            }
        }
    }
}
