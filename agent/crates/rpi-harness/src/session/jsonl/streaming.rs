//! Bounded physical-line scanner and atomic span-copy repair. Offsets always
//! refer to the reader generation that validated them, never the published path.
use super::*;
use rpi_tools::env::ReadFile;
const CHUNK: usize = 1024 * 1024;

struct Scanner {
    reader: Arc<dyn ReadFile>,
    length: u64,
    fetched: u64,
    offset: u64,
    buffer: Vec<u8>,
    recycled_line: Vec<u8>,
    cursor: usize,
    hash: Sha256,
    hash_enabled: bool,
    policy: ReadPolicy,
}
impl Scanner {
    fn new(reader: Arc<dyn ReadFile>, length: u64) -> Self {
        Self {
            reader,
            length,
            fetched: 0,
            offset: 0,
            buffer: Vec::new(),
            recycled_line: Vec::new(),
            cursor: 0,
            hash: Sha256::new(),
            hash_enabled: true,
            policy: ReadPolicy::Restore,
        }
    }
    fn recycle(&mut self, mut line: Vec<u8>) {
        // One unusually large row must not become a permanently retained
        // scratch allocation for the rest of the history.
        if line.capacity() <= CHUNK {
            line.clear();
            self.recycled_line = line;
        }
    }
    async fn next(&mut self) -> SessionResult<Option<(u64, Vec<u8>, bool, bool)>> {
        let offset = self.offset;
        let mut line = std::mem::take(&mut self.recycled_line);
        loop {
            self.policy.check(0)?;
            if self.cursor < self.buffer.len() {
                let mut remainder = &self.buffer[self.cursor..];
                // The slice BufRead implementation uses the standard library's
                // optimized delimiter search without validating partial UTF-8.
                let consumed = std::io::BufRead::read_until(&mut remainder, b'\n', &mut line)
                    .expect("reading a byte slice is infallible");
                self.cursor += consumed;
                self.offset += consumed as u64;
                if line.last() == Some(&b'\n') {
                    line.pop();
                    return Ok(Some((offset, line, self.offset == self.length, true)));
                }
            }
            if self.fetched == self.length {
                return Ok((!line.is_empty()).then_some((offset, line, true, false)));
            }
            let count = (self.length - self.fetched).min(CHUNK as u64) as usize;
            self.buffer = file_result(
                self.reader.read_range(self.fetched, count).await,
                "scan session history",
            )?;
            self.policy.check(0)?;
            if self.buffer.len() != count {
                return Err(SessionError::storage("short session history scan"));
            }
            if self.hash_enabled {
                self.hash.update(&self.buffer);
            }
            self.fetched += count as u64;
            self.cursor = 0;
        }
    }
}

async fn copy_range(
    fs: &Arc<dyn FileSystem>,
    target: &str,
    reader: &Arc<dyn ReadFile>,
    offset: u64,
    length: u64,
    hash: &mut Sha256,
) -> SessionResult<()> {
    let mut copied = 0;
    while copied < length {
        let count = (length - copied).min(CHUNK as u64) as usize;
        let bytes = file_result(
            reader.read_range(offset + copied, count).await,
            "copy session history span",
        )?;
        if bytes.len() != count {
            return Err(SessionError::storage("short session history span"));
        }
        hash.update(&bytes);
        file_result(
            fs.append_file(target, FileContent::Bytes(bytes), None)
                .await,
            "stage session history span",
        )?;
        copied += count as u64;
    }
    Ok(())
}
async fn hash_reader(reader: &Arc<dyn ReadFile>, length: u64) -> SessionResult<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < length {
        let count = (length - offset).min(CHUNK as u64) as usize;
        let bytes = file_result(
            reader.read_range(offset, count).await,
            "hash session history",
        )?;
        if bytes.len() != count {
            return Err(SessionError::storage("short session history hash read"));
        }
        hash.update(&bytes);
        offset += count as u64;
    }
    Ok(hash.finalize().into())
}

impl JsonlSessionStorage {
    pub(super) async fn load_streaming(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        reader: Arc<dyn ReadFile>,
        length: u64,
        policy: ReadPolicy,
    ) -> SessionResult<Self> {
        let diagnostics = crate::session::diagnostics::RecoveryPhase::start("history_streaming");
        policy.check(0)?;
        let allow_repair = policy.repairs();
        let strict = policy.strict();
        let mut scan = Scanner::new(reader.clone(), length);
        scan.policy = policy;
        let Some((_, header_raw, _, header_newline)) = scan.next().await? else {
            return Err(invalid_file(path, 1, &"is missing a header"));
        };
        let header_line = std::str::from_utf8(&header_raw)
            .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
        if header_line.is_empty() {
            return Err(invalid_file(path, 1, &"is missing a header"));
        }
        if strict && !header_newline {
            return Err(SessionError::invalid_entry(
                "resource transition requires complete unrecovered history",
            ));
        }
        let header = parse_header(header_line).map_err(|e| invalid_file(path, 1, &e))?;
        // This whole-file digest is ONLY the guard for a future legacy format
        // upgrade, not a persisted checksum of the source. Modern histories do
        // not upgrade, and FactsOnly exports no writable storage capability.
        // All retained cold rows still keep their per-location SHA-256 checks.
        let needs_legacy_digest =
            header.progress_format.is_none() && !matches!(policy, ReadPolicy::FactsOnly);
        scan.hash_enabled = needs_legacy_digest;
        if header.progress_format.is_some()
            && !file_result(
                fs.exists(&format!("{path}.progress.jsonl"), None).await,
                "check required progress companion",
            )?
        {
            return Err(SessionError::invalid_entry("required progress companion is missing; restore the complete session, not only its history"));
        }
        let info = file_result(fs.file_info(path, None).await, "read session metadata")?;
        let mut storage = Self::new(
            fs.clone(),
            metadata_from_header(&header, path, info.mtime_ms),
            clock,
            ids,
        );
        storage.progress.required = header.progress_format.is_some();
        let mut spans = vec![(0u64, header_raw.len() as u64)];
        let mut seen: HashMap<u64, (u64, usize)> = HashMap::new();
        let mut repair = false;
        let mut terminated = header_newline;
        let mut line_no = 1;
        while let Some((offset, raw, last, newline)) = scan.next().await? {
            line_no += 1;
            policy.check((line_no - 1) as usize)?;
            terminated = newline;
            if strict && !newline {
                return Err(SessionError::invalid_entry(
                    "resource transition requires complete unrecovered history",
                ));
            }
            let line = std::str::from_utf8(&raw)
                .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
            if let Some(frame) = super::super::cold_decode::parse_cold_frame(line) {
                let mut state = storage.state.lock().await;
                // Stale sequences must still take the canonical full-decode
                // semantic comparison below (key order/whitespace may differ).
                if frame.seq > state.sequence() {
                    if frame.seq > state.next_sequence() {
                        state.resync_sequence(frame.seq - 1);
                    }
                    let seq = frame.seq;
                    // FactsOnly never escapes read_facts as a storage handle.
                    // Keep ID/sequence/lane validation and frame retirement,
                    // but there is no frame-read capability to index or hash.
                    // Stale seqs and unrecognized rows still use the unchanged
                    // full decoder/reducer path below.
                    let applied = if matches!(policy, ReadPolicy::FactsOnly) {
                        state.reserve_loaded_frame(frame)
                    } else {
                        state.apply_loaded_frame(frame, &reader, offset, &raw)
                    };
                    applied.map_err(|error| {
                        if error.code == SessionErrorCode::InvalidEntry {
                            invalid_file(path, line_no, &error)
                        } else {
                            error
                        }
                    })?;
                    seen.insert(seq, (offset, raw.len()));
                    spans.push((offset, raw.len() as u64));
                    scan.recycle(raw);
                    continue;
                }
            }
            match parse_mutation(line) {
                Ok(mutation) => {
                    let mut state = storage.state.lock().await;
                    if mutation.seq() <= state.sequence() {
                        let duplicate = if let Some(&(previous, count)) = seen.get(&mutation.seq())
                        {
                            let bytes = file_result(
                                reader.read_range(previous, count).await,
                                "compare duplicate session mutation",
                            )?;
                            let prior = std::str::from_utf8(&bytes)
                                .ok()
                                .and_then(|line| parse_mutation(line).ok());
                            prior.as_ref() == Some(&mutation)
                        } else {
                            false
                        };
                        if duplicate {
                            if strict {
                                return Err(SessionError::invalid_entry(
                                    "resource transition rejects duplicate history sequences",
                                ));
                            }
                            repair = true;
                            scan.recycle(raw);
                            continue;
                        }
                        return Err(invalid_file(
                            path,
                            line_no,
                            &format!("conflicting mutations share sequence {}", mutation.seq()),
                        ));
                    }
                    if mutation.seq() > state.next_sequence() {
                        state.resync_sequence(mutation.seq() - 1);
                    }
                    let seq = mutation.seq();
                    let mutation = chain_onto_lane_leaf(mutation, &state);
                    state
                        .apply_loaded(mutation, &reader, offset, &raw)
                        .map_err(|error| {
                            if error.code == SessionErrorCode::InvalidEntry {
                                invalid_file(path, line_no, &error)
                            } else {
                                error
                            }
                        })?;
                    seen.insert(seq, (offset, raw.len()));
                    spans.push((offset, raw.len() as u64));
                }
                Err(error)
                    if !strict
                        && error.kind == JsonlDecodeErrorKind::Schema
                        && is_assistant_frame_append(line) =>
                {
                    tracing::warn!(line = line_no, "skipping undecodable assistant frame");
                    spans.push((offset, raw.len() as u64));
                }
                Err(error) if !strict && last && error.kind == JsonlDecodeErrorKind::Syntax => {
                    repair = true;
                    break;
                }
                Err(error) => return Err(invalid_file(path, line_no, &error)),
            }
            scan.recycle(raw);
        }
        if repair && allow_repair {
            let digest = Arc::new(std::sync::Mutex::new(None));
            let output = digest.clone();
            publish_file_atomically(&fs, path, move |fs, temp| {
                Box::pin(async move {
                    file_result(
                        fs.write_file(temp, FileContent::Bytes(Vec::new()), None)
                            .await,
                        "stage session repair",
                    )?;
                    let mut hash = Sha256::new();
                    for (offset, count) in spans {
                        copy_range(fs, temp, &reader, offset, count, &mut hash).await?;
                        file_result(
                            fs.append_file(temp, FileContent::Bytes(vec![b'\n']), None)
                                .await,
                            "stage repaired newline",
                        )?;
                        hash.update(b"\n");
                    }
                    *output.lock().unwrap() = Some(<[u8; 32]>::from(hash.finalize()));
                    Ok(())
                })
            })
            .await?;
            if needs_legacy_digest {
                storage.legacy_history_digest = *digest.lock().unwrap();
            }
        } else {
            if !terminated && allow_repair {
                file_result(
                    fs.append_file(path, FileContent::Text("\n".into()), None)
                        .await,
                    "repair unterminated session tail",
                )?;
                scan.hash.update(b"\n");
            }
            if needs_legacy_digest {
                storage.legacy_history_digest = Some(scan.hash.finalize().into());
            }
        }
        policy.check((line_no - 1) as usize)?;
        diagnostics.finish(length as usize, (line_no - 1) as usize);
        Ok(storage)
    }

    /// Returns false only for compatibility filesystems without stable ranged
    /// readers; their original eager upgrade implementation remains isolated.
    pub(super) async fn upgrade_streaming(&mut self, expected: [u8; 32]) -> SessionResult<bool> {
        let path = self.metadata.path.clone();
        let Some(reader) = file_result(
            self.fs.open_read_file(&path).await,
            "open progress upgrade history",
        )?
        else {
            return Ok(false);
        };
        let Some(length) = file_result(reader.byte_len().await, "stat progress upgrade history")?
        else {
            return Ok(false);
        };
        if hash_reader(&reader, length).await? != expected {
            return Err(SessionError::invalid_entry(
                "session history changed before progress upgrade; reopen session",
            ));
        }
        let mut scan = Scanner::new(reader.clone(), length);
        let Some((_, raw, _, newline)) = scan.next().await? else {
            return Err(SessionError::invalid_entry(
                "progress upgrade requires a complete header",
            ));
        };
        if !newline {
            return Err(SessionError::invalid_entry(
                "progress upgrade requires a complete header",
            ));
        }
        let header_line = std::str::from_utf8(&raw)
            .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
        let header = parse_header(header_line).map_err(|e| invalid_file(&path, 1, &e))?;
        if header.id != self.metadata.id || header.progress_format.is_some() || header.version != 4
        {
            return Err(SessionError::invalid_entry(
                "session header changed before progress upgrade",
            ));
        }
        let mut value: serde_json::Value = serde_json::from_str(header_line)
            .map_err(|e| SessionError::invalid_entry(e.to_string()))?;
        value["progressFormat"] = super::super::progress::FORMAT.into();
        let upgraded = format!(
            "{}\n",
            serde_json::to_string(&value).map_err(|e| SessionError::storage(e.to_string()))?
        );
        let body_offset = raw.len() as u64 + 1;
        let progress = super::super::progress::JsonlProgressStore::new(
            self.fs.clone(),
            &path,
            &header.id,
            self.clock.clone(),
        );
        progress.initialize_legacy_upgrade().await?;
        *self.append.lock().await = None;
        let history_path = path.clone();
        publish_file_atomically(&self.fs, &path, move |fs, temp| {
            Box::pin(async move {
                file_result(
                    fs.write_file(temp, FileContent::Text(upgraded), None).await,
                    "stage progress upgrade header",
                )?;
                copy_range(
                    fs,
                    temp,
                    &reader,
                    body_offset,
                    length - body_offset,
                    &mut Sha256::new(),
                )
                .await?;
                // Re-open the path too: hashing the old reader alone cannot detect
                // an atomic path replacement by a foreign writer.
                let current = file_result(
                    fs.open_read_file(&history_path).await,
                    "recheck history for progress upgrade",
                )?
                .ok_or_else(|| SessionError::storage("missing upgrade recheck reader"))?;
                let current_len =
                    file_result(current.byte_len().await, "stat upgrade recheck reader")?
                        .ok_or_else(|| SessionError::storage("missing upgrade recheck length"))?;
                if current_len != length || hash_reader(&current, current_len).await? != expected {
                    return Err(SessionError::invalid_entry(
                        "session history changed during progress upgrade; reopen session",
                    ));
                }
                Ok(())
            })
        })
        .await?;
        self.progress = progress;
        self.legacy_history_digest = None;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod tests;
