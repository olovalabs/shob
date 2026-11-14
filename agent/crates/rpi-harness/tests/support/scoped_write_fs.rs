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
        if self.gate.fail_stage.swap(false, Ordering::SeqCst) && p.ends_with(".progress.jsonl.tmp")
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
    async fn open_append_file(&self, p: &str) -> Result<Option<Box<dyn AppendFile>>, FileError> {
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
