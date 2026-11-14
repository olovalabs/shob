use super::*;
use crate::session::{
    jsonl::cold_frames::JsonlPayload,
    memory::{CounterIdGenerator, FakeClock},
    state::SessionState,
    types::{EntryOrder, ForkPosition, MessageSummaryQuery, MessageWindowQuery, ProvisionedKind},
};
use std::sync::atomic::AtomicUsize;

struct StrictReader {
    inner: Arc<dyn ReadFile>,
    calls: AtomicUsize,
    ranges: std::sync::Mutex<Vec<(u64, usize)>>,
}
#[async_trait]
impl ReadFile for StrictReader {
    async fn read_all(&self) -> Result<Vec<u8>, rpi_tools::FileError> {
        panic!("stable JSONL history reader must never use read_all")
    }
    async fn byte_len(&self) -> Result<Option<u64>, rpi_tools::FileError> {
        self.inner.byte_len().await
    }
    async fn read_range(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, rpi_tools::FileError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.ranges.lock().unwrap().push((offset, length));
        self.inner.read_range(offset, length).await
    }
}
fn header() -> JsonlV4Header {
    JsonlV4Header::new("stream-test".into(), 1, "/cwd".into(), None, None, None)
}
fn message(id: &str, seq: u64, parent: Option<&str>, text: &str) -> SessionMutation {
    let entry = provisioned_into_entry(
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(text, seq as i64).into(),
                terminate: None,
            },
        },
        seq,
        parent.map(str::to_owned),
        seq as i64,
    );
    SessionMutation::Entry {
        seq,
        timestamp: seq as i64,
        lane: Some("main".into()),
        entry,
    }
}
fn is_cold(storage: &tokio::sync::MutexGuard<'_, ResidentState>, id: &str) -> bool {
    matches!(storage.entry_ref(id), Some(JsonlPayload::Cold { .. }))
}
async fn strict_load(
    fs: Arc<dyn FileSystem>,
    path: &str,
) -> (JsonlSessionStorage, Arc<StrictReader>) {
    let inner = fs.open_read_file(path).await.unwrap().unwrap();
    let reader = Arc::new(StrictReader {
        inner,
        calls: AtomicUsize::new(0),
        ranges: std::sync::Mutex::new(Vec::new()),
    });
    let length = reader.byte_len().await.unwrap().unwrap();
    let storage = JsonlSessionStorage::load_streaming(
        fs,
        path,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
        reader.clone(),
        length,
        ReadPolicy::Restore,
    )
    .await
    .unwrap();
    (storage, reader)
}

#[tokio::test]
async fn strict_cold_replay_obeys_scan_budget_and_metadata_never_hydrates_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mut bytes = encode_header(&header());
    for seq in 1..=12 {
        let parent = (seq > 1).then(|| format!("e{}", seq - 1));
        bytes.push_str(&encode_mutation(&message(
            &format!("e{seq}"),
            seq,
            parent.as_deref(),
            &"x".repeat(CHUNK / 3),
        )));
    }
    fs.write_file("strict.jsonl", FileContent::Text(bytes.clone()), None)
        .await
        .unwrap();
    let reader = Arc::new(StrictReader {
        inner: fs.open_read_file("strict.jsonl").await.unwrap().unwrap(),
        calls: AtomicUsize::new(0),
        ranges: std::sync::Mutex::new(Vec::new()),
    });
    let storage = JsonlSessionStorage::load_streaming(
        fs.clone(),
        "strict.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
        reader.clone(),
        bytes.len() as u64,
        ReadPolicy::ResourceTransitionStrict,
    )
    .await
    .unwrap();
    let calls = reader.calls.load(Ordering::Relaxed);
    assert_eq!(calls, bytes.len().div_ceil(CHUNK));
    assert!(reader
        .ranges
        .lock()
        .unwrap()
        .iter()
        .all(|(_, count)| *count <= CHUNK));
    for seq in 1..=12 {
        assert!(is_cold(&storage.state.lock().await, &format!("e{seq}")));
    }
    storage.facts().await.unwrap();
    storage.branch_overview("main").await.unwrap();
    storage.get_lanes().await.unwrap();
    assert_eq!(
        reader.calls.load(Ordering::Relaxed),
        calls,
        "facts and metadata must not read any historical message payload"
    );
    assert_eq!(
        fs.read_text_file("strict.jsonl", None).await.unwrap(),
        bytes
    );
}

#[tokio::test]
async fn os_ranged_load_large_unicode_rechain_duplicate_and_repair_keep_source_generation() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let first = message(
        "e1",
        1,
        None,
        &format!(
            "{}🌊\r\n{}",
            "x".repeat(CHUNK - 220),
            "🌊".repeat(CHUNK / 4)
        ),
    );
    // Forward gaps and a divergent parent are accepted; the cold reference
    // must apply the repaired parent without changing the original source row.
    let second = message("e2", 3, None, "second");
    let mut bytes = encode_header(&header()).replace('\n', "\r\n");
    bytes.push_str(&encode_mutation(&first).replace('\n', "\r\n"));
    bytes.push_str(&encode_mutation(&second));
    // Same mutation, distinct raw encoding: duplicate comparison is semantic.
    bytes.push_str("  ");
    bytes.push_str(encode_mutation(&first).trim_end());
    bytes.push_str(" \n");
    bytes.push_str("{\"kind\":");
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, reader) = strict_load(fs.clone(), "history.jsonl").await;
    assert!(reader.calls.load(Ordering::Relaxed) >= 3);
    assert!(is_cold(&storage.state.lock().await, "e1"));
    assert!(is_cold(&storage.state.lock().await, "e2"));
    assert_eq!(
        storage.get_entry("e2").await.unwrap().unwrap().parent_id(),
        Some("e1")
    );
    let original = storage.get_entry("e1").await.unwrap().unwrap();
    // Replace the published path again. The loaded offsets still belong to
    // the retired, stable source reader, not this new path's shorter bytes.
    let replacement = format!(
        "{}{}",
        encode_header(&header()),
        encode_mutation(&message("other", 1, None, "replacement"))
    );
    fs.write_file("replacement.tmp", FileContent::Text(replacement), None)
        .await
        .unwrap();
    fs.rename_file("replacement.tmp", "history.jsonl", None)
        .await
        .unwrap();
    assert_eq!(storage.get_entry("e1").await.unwrap().unwrap(), original);
    let appended = storage
        .append_entry(
            ProvisionedEntry {
                id: "e3".into(),
                kind: ProvisionedKind::Message {
                    message: rpi_ai::UserMessage::new("fresh", 4).into(),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    assert_eq!(appended.parent_id(), Some("e2"));
    assert!(is_cold(&storage.state.lock().await, "e3"));
    assert_eq!(storage.get_entry("e3").await.unwrap().unwrap(), appended);
    assert_eq!(storage.get_entry("e1").await.unwrap().unwrap(), original);
}

#[tokio::test]
async fn cold_entry_queries_windows_context_log_and_fork_match_eager() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mut eager = SessionState::new();
    let mut bytes = encode_header(&header());
    for seq in 1..=12 {
        let id = format!("e{seq}");
        let parent = (seq > 1).then(|| format!("e{}", seq - 1));
        let mutation = message(&id, seq, parent.as_deref(), &format!("user {seq} 🌊"));
        bytes.push_str(&encode_mutation(&mutation));
        eager.apply_mutation(mutation).unwrap();
    }
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, _) = strict_load(fs.clone(), "history.jsonl").await;
    for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
        for limit in [None, Some(1), Some(5)] {
            let query = EntryQuery {
                order: Some(order),
                limit,
                ..Default::default()
            };
            assert_eq!(
                storage.find_entries(&query).await.unwrap(),
                eager.find_entries(&query).unwrap()
            );
            let bounds = BranchBounds::default();
            assert_eq!(
                storage
                    .find_entries_on_branch(&query, &bounds, "e8")
                    .await
                    .unwrap(),
                eager.find_entries_on_branch(&query, &bounds, "e8").unwrap()
            );
        }
    }
    let summary = MessageSummaryQuery {
        limit: 3,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(storage.message_summaries("main", &summary).await.unwrap()).unwrap(),
        serde_json::to_value(eager.message_summaries("main", &summary).unwrap()).unwrap()
    );
    for query in [
        MessageWindowQuery {
            anchor_id: Some("e6".into()),
            limit: 4,
            ..Default::default()
        },
        MessageWindowQuery {
            before_id: Some("e6".into()),
            limit: 2,
            ..Default::default()
        },
        MessageWindowQuery {
            after_id: Some("e6".into()),
            limit: 2,
            ..Default::default()
        },
    ] {
        let actual = storage.message_window("main", &query).await.unwrap();
        let expected = eager.message_window("main", &query).unwrap();
        assert_eq!(actual.entries, expected.entries);
        assert_eq!(actual.has_more_before, expected.has_more_before);
        assert_eq!(actual.has_more_after, expected.has_more_after);
        assert_eq!(actual.leaf_id, expected.leaf_id);
    }
    assert_eq!(
        storage
            .context_on_branch(&BranchBounds::default(), "e12")
            .await
            .unwrap()
            .messages,
        eager
            .context_on_branch(&BranchBounds::default(), "e12")
            .unwrap()
            .messages
    );
    assert_eq!(
        storage.get_log(&LogOptions::default()).await.unwrap(),
        eager.get_log(&LogOptions::default()).unwrap()
    );
    storage.set_name(Some("fork name")).await.unwrap();
    storage.set_label("e6", Some("label")).await.unwrap();
    let options = ForkOptions::Branch {
        entry_id: Some("e8".into()),
        position: Some(ForkPosition::At),
    };
    let fork = storage
        .fork(
            "fork.jsonl",
            header(),
            &options,
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        fork.find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        8
    );
    assert_eq!(fork.get_name().await.unwrap().as_deref(), Some("fork name"));
    assert_eq!(
        fork.get_label("e6").await.unwrap().as_deref(),
        Some("label")
    );
    assert!(is_cold(&fork.state.lock().await, "e1"));
}

#[tokio::test]
async fn scanner_delimiters_preserve_raw_lines_offsets_flags_and_digest() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mut cases = vec![Vec::new(), b"\n\n".to_vec(), b"one\r\ntwo".to_vec()];
    for newline_offset in [CHUNK - 1, CHUNK, CHUNK + 1] {
        let mut bytes = vec![b'a'; newline_offset - 1];
        bytes.extend_from_slice(b"\r\n\n");
        // Keep raw scanning distinct from UTF-8/schema validation. Include
        // split multibyte text, an invalid byte and an unterminated tail.
        bytes.extend_from_slice("🌊".repeat(CHUNK / 4 + 1).as_bytes());
        bytes.extend_from_slice(b"\xff\nlast\r");
        cases.push(bytes);
    }
    for (index, bytes) in cases.into_iter().enumerate() {
        let path = format!("scanner-{index}");
        fs.write_file(&path, FileContent::Bytes(bytes.clone()), None)
            .await
            .unwrap();
        let reader = fs.open_read_file(&path).await.unwrap().unwrap();
        let mut scanner = Scanner::new(reader, bytes.len() as u64);
        let mut offset = 0;
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            let newline = part.last() == Some(&b'\n');
            let body = if newline {
                &part[..part.len() - 1]
            } else {
                part
            };
            let end = offset + part.len() as u64;
            assert_eq!(
                scanner.next().await.unwrap().unwrap(),
                (offset, body.to_vec(), end == bytes.len() as u64, newline),
                "case={index}, offset={offset}"
            );
            offset = end;
        }
        assert!(scanner.next().await.unwrap().is_none());
        assert_eq!(scanner.offset, bytes.len() as u64);
        assert_eq!(
            <[u8; 32]>::from(scanner.hash.finalize()),
            <[u8; 32]>::from(Sha256::digest(&bytes))
        );
    }
}

#[tokio::test]
async fn scanner_chunk_boundaries_utf8_crlf_and_final_newline() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mutation = message("large", 1, None, &"🌊".repeat(CHUNK));
    let mut bytes = encode_header(&header());
    bytes.push_str(encode_mutation(&mutation).trim_end());
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, _) = strict_load(fs.clone(), "history.jsonl").await;
    let SessionMutation::Entry { entry, .. } = mutation else {
        unreachable!()
    };
    assert_eq!(storage.get_entry("large").await.unwrap().unwrap(), entry);
    assert!(fs
        .read_binary_file("history.jsonl", None)
        .await
        .unwrap()
        .ends_with(b"\n"));
    assert!(is_cold(&storage.state.lock().await, "large"));
}

#[tokio::test]
async fn bounded_header_lookup_does_not_bypass_full_history_utf8_validation() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let encoded_header = encode_header(&header());
    let mut bytes = encoded_header.as_bytes().to_vec();
    bytes.extend_from_slice(encode_mutation(&message("valid", 1, None, "synthetic")).as_bytes());
    // A complete malformed line, not a recoverable torn final write.
    bytes.extend_from_slice(b"\xff\n");
    fs.write_file("history.jsonl", FileContent::Bytes(bytes), None)
        .await
        .unwrap();
    assert_eq!(
        fs.read_text_lines("history.jsonl", Some(1), None)
            .await
            .unwrap(),
        [encoded_header.trim_end_matches('\n')]
    );
    let result = JsonlSessionStorage::load(
        fs,
        "history.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await;
    let error = result
        .err()
        .expect("full replay must reject invalid history");
    assert!(error.message.contains("invalid session UTF-8"), "{error}");
}

#[tokio::test]
async fn context_uses_metadata_before_compaction_and_payload_filter_precedes_limit() {
    use crate::session::types::{
        ActiveToolsEntry, CompactionEntry, CustomEntry, EntryBase, MessageEntry, ModelChangeEntry,
        ThinkingLevelEntry,
    };
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let base = |seq: u64, kind: &str| EntryBase {
        entry_type: kind.into(),
        id: format!("e{seq}"),
        seq,
        parent_id: (seq > 1).then(|| format!("e{}", seq - 1)),
        timestamp: seq as i64,
    };
    let mut entries = vec![
        Entry::ModelChange(ModelChangeEntry {
            base: base(1, "model_change"),
            provider: "provider".into(),
            model_id: "model".into(),
        }),
        Entry::ThinkingLevel(ThinkingLevelEntry {
            base: base(2, "thinking_level_change"),
            thinking_level: "high".into(),
        }),
        Entry::ActiveTools(ActiveToolsEntry {
            base: base(3, "active_tools_change"),
            active_tool_names: vec!["read".into()],
        }),
    ];
    let SessionMutation::Entry { entry: old, .. } =
        message("e4", 4, Some("e3"), &"old".repeat(CHUNK))
    else {
        unreachable!()
    };
    entries.push(old);
    entries.push(Entry::Compaction(CompactionEntry {
        base: base(5, "compaction"),
        summary: "summary".into(),
        retained_tail: Vec::new(),
        tokens_before: 100,
        details: None,
        usage: None,
    }));
    let SessionMutation::Entry { entry: tail, .. } = message("e6", 6, Some("e5"), "tail") else {
        unreachable!()
    };
    entries.push(tail);
    entries.push(Entry::Custom(CustomEntry {
        base: base(7, "custom"),
        custom_type: "extension".into(),
        data: Some(serde_json::json!({"large": "payload".repeat(CHUNK/7)})),
    }));
    for seq in 8..=10 {
        entries.push(Entry::Message(MessageEntry { base: base(seq,"message"), terminate: None,
            message: rpi_agent::message::AgentMessage::Custom(rpi_agent::message::CustomMessage::new("custom", Vec::new(), serde_json::json!({"__rpiOutbox":{"key": if seq == 8 { "wanted" } else { "other" }}}),seq as i64)),
        }));
    }
    let mut eager = SessionState::new();
    let mut bytes = encode_header(&header());
    let mut context_start = 0;
    for entry in entries {
        if entry.seq() == 5 {
            context_start = bytes.len() as u64;
        }
        let mutation = SessionMutation::Entry {
            seq: entry.seq(),
            timestamp: entry.base().timestamp,
            lane: Some("main".into()),
            entry,
        };
        bytes.push_str(&encode_mutation(&mutation));
        eager.apply_mutation(mutation).unwrap();
    }
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, reader) = strict_load(fs.clone(), "history.jsonl").await;
    let before = reader.calls.load(Ordering::Relaxed);
    let context = storage
        .context_on_branch(&BranchBounds::default(), "e7")
        .await
        .unwrap();
    let expected = eager
        .context_on_branch(&BranchBounds::default(), "e7")
        .unwrap();
    assert_eq!(context.messages, expected.messages);
    assert_eq!(context.model, expected.model);
    assert_eq!(context.thinking_level, expected.thinking_level);
    assert_eq!(context.active_tool_names, expected.active_tool_names);
    // Only compaction, its tail and the custom entry are hydrated. Model,
    // thinking and active-tools state come from small full-path facts.
    // Compaction and its tail coalesce. The custom row plus its JSON envelope
    // exceeds 1 MiB and is hydrated alone, through two bounded physical reads.
    assert_eq!(reader.calls.load(Ordering::Relaxed) - before, 3);
    let ranges = reader.ranges.lock().unwrap();
    let selected_ranges = &ranges[ranges.len() - 3..];
    assert_eq!(selected_ranges[0].0, context_start);
    assert!(selected_ranges
        .iter()
        .all(|(offset, length)| *offset >= context_start && *length <= CHUNK));
    drop(ranges);
    let before = reader.calls.load(Ordering::Relaxed);
    let overview = storage.branch_overview("main").await.unwrap();
    assert_eq!(overview.model, expected.model);
    assert_eq!(overview.thinking_level, expected.thinking_level);
    assert_eq!(overview.active_tool_names, expected.active_tool_names);
    assert_eq!(
        reader.calls.load(Ordering::Relaxed),
        before,
        "overview must not hydrate historical payloads"
    );
    assert!(storage.branch_overview("missing-lane").await.is_err());
    let query = EntryQuery {
        outbox_key: Some("wanted".into()),
        limit: Some(1),
        ..Default::default()
    };
    let actual = storage.find_entries(&query).await.unwrap();
    assert_eq!(actual, eager.find_entries(&query).unwrap());
    assert_eq!(actual[0].id(), "e8");
    assert!(is_cold(&storage.state.lock().await, "e7"));
    assert_eq!(
        storage.get_entry("e7").await.unwrap().as_ref(),
        eager.get_entry("e7")
    );
    // A source change must be an error, never a missing entry or fake payload.
    let inner = fs.open_read_file("history.jsonl").await.unwrap().unwrap();
    drop(inner);
    fs.truncate_file("history.jsonl", 1, None).await.unwrap();
    assert!(storage.get_entry("e7").await.is_err());
}

fn cold_frame_row(seq: u64, op: &str, payload: serde_json::Value) -> String {
    serde_json::json!({
        "kind": "record", "type": "assistant_frame", "id": format!("f{seq}"),
        "seq": seq, "lane": "main", "timestamp": -(seq as i64),
        "runId": "run", "streamIndex": 0, "op": op, "frame": payload
    })
    .to_string()
}

async fn load_frame_history(
    fs: Arc<dyn FileSystem>,
    path: &str,
    policy: ReadPolicy,
) -> (SessionResult<JsonlSessionStorage>, Arc<StrictReader>) {
    let reader = Arc::new(StrictReader {
        inner: fs.open_read_file(path).await.unwrap().unwrap(),
        calls: AtomicUsize::new(0),
        ranges: std::sync::Mutex::new(Vec::new()),
    });
    let length = reader.byte_len().await.unwrap().unwrap();
    let result = JsonlSessionStorage::load_streaming(
        fs,
        path,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
        reader.clone(),
        length,
        policy,
    )
    .await;
    (result, reader)
}

#[tokio::test]
async fn cold_frame_duplicate_sequences_compare_semantics_and_reject_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    for op in ["append", "clear_stream", "clear_run"] {
        let first = cold_frame_row(1, op, serde_json::json!({"a": 1, "z": [true, null]}));
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        let fields = value
            .as_object()
            .unwrap()
            .iter()
            .rev()
            .map(|(key, value)| format!("{key:?}: {value}"))
            .collect::<Vec<_>>();
        let duplicate = format!(" {{ {} }} ", fields.join(", "));
        assert_ne!(first, duplicate);
        assert_eq!(
            parse_mutation(&first).unwrap(),
            parse_mutation(&duplicate).unwrap()
        );
        assert!(crate::session::jsonl::cold_decode::parse_cold_frame(&first).is_some());
        assert!(crate::session::jsonl::cold_decode::parse_cold_frame(&duplicate).is_some());
        // The first row is fresh and fast; its stale equivalent must re-read
        // that original row through the full codec, even after a later seq.
        let next = cold_frame_row(2, "append", serde_json::json!("later"));
        let original = format!(
            "{}{}\n{}\n{}\n",
            encode_header(&header()),
            first,
            next,
            duplicate
        );
        fs.write_file("frames.jsonl", FileContent::Text(original.clone()), None)
            .await
            .unwrap();
        let (strict, _) = load_frame_history(
            fs.clone(),
            "frames.jsonl",
            ReadPolicy::ResourceTransitionStrict,
        )
        .await;
        assert!(strict
            .err()
            .unwrap()
            .message
            .contains("duplicate history sequences"));
        let (restored, reader) =
            load_frame_history(fs.clone(), "frames.jsonl", ReadPolicy::Restore).await;
        let restored = restored.unwrap();
        assert_eq!(restored.state.lock().await.sequence(), 2);
        let mut expected = SessionState::new();
        expected
            .apply_mutation(parse_mutation(&first).unwrap())
            .unwrap();
        expected
            .apply_mutation(parse_mutation(&next).unwrap())
            .unwrap();
        let query = crate::session::types::RecordQuery::default();
        assert_eq!(
            restored.find_records(&query).await.unwrap(),
            expected.find_records(&query).unwrap()
        );
        assert!(
            reader.calls.load(Ordering::Relaxed) > 1,
            "duplicates require a reference-row read"
        );
        let repaired = fs.read_text_file("frames.jsonl", None).await.unwrap();
        assert_eq!(
            repaired,
            format!("{}{}\n{}\n", encode_header(&header()), first, next)
        );
        let mut conflicting = value;
        conflicting["frame"] = serde_json::json!({"a": 2, "z": [true, null]});
        fs.write_file(
            "conflict.jsonl",
            FileContent::Text(format!(
                "{}{}\n{}\n",
                encode_header(&header()),
                first,
                conflicting
            )),
            None,
        )
        .await
        .unwrap();
        let (result, _) =
            load_frame_history(fs.clone(), "conflict.jsonl", ReadPolicy::Restore).await;
        let error = result.err().unwrap();
        assert!(
            error
                .message
                .contains("conflicting mutations share sequence 1"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn cold_frame_reordered_fields_and_duplicate_known_keys_keep_full_codec_authority() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    // Frame before discriminators; unknown duplicate keys do not require fallback.
    let reordered = r#"{"frame":{"z":"🌊","a":[null,true]},"op":"append","streamIndex":3,"runId":"run","timestamp":-1,"lane":"main","seq":1,"id":"f1","extra":1,"extra":2,"type":"assistant_frame","kind":"record"}"#;
    // Known duplicate keys must fall back to Value's last-key-wins behavior,
    // including an earlier invalid type and an earlier conflicting identity.
    let canonical = cold_frame_row(2, "append", serde_json::json!({"text": "kept"}));
    let fallback = canonical.replacen(
        '{',
        r#"{"streamIndex":"invalid","id":"shadow","frame":"discarded","#,
        1,
    );
    assert!(crate::session::jsonl::cold_decode::parse_cold_frame(reordered).is_some());
    assert!(crate::session::jsonl::cold_decode::parse_cold_frame(&fallback).is_none());
    assert_eq!(
        parse_mutation(&fallback).unwrap(),
        parse_mutation(&canonical).unwrap()
    );
    let bytes = format!("{}{}\n{}\n", encode_header(&header()), reordered, fallback);
    fs.write_file("fields.jsonl", FileContent::Text(bytes.clone()), None)
        .await
        .unwrap();
    let (result, reader) = load_frame_history(
        fs.clone(),
        "fields.jsonl",
        ReadPolicy::ResourceTransitionStrict,
    )
    .await;
    let storage = result.unwrap();
    assert_eq!(reader.calls.load(Ordering::Relaxed), 1);
    {
        let state = storage.state.lock().await;
        assert_eq!(state.sequence(), 2);
        assert!(state.validate_unused_id("shadow").is_ok());
        assert!(state.validate_unused_id("f2").is_err());
    }
    let mut eager = SessionState::new();
    for line in [reordered, fallback.as_str()] {
        eager.apply_mutation(parse_mutation(line).unwrap()).unwrap();
    }
    let query = crate::session::types::RecordQuery::default();
    assert_eq!(
        storage.find_records(&query).await.unwrap(),
        eager.find_records(&query).unwrap()
    );
    assert_eq!(
        fs.read_text_file("fields.jsonl", None).await.unwrap(),
        bytes
    );
}

#[tokio::test]
async fn fresh_fast_frames_cross_chunk_boundaries_and_recover_complete_or_torn_tails() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let template = cold_frame_row(1, "append", serde_json::json!({"delta": "BOUNDARY"}));
    let padding = CHUNK - encode_header(&header()).len() - template.find("BOUNDARY").unwrap() - 1;
    let large = cold_frame_row(
        1,
        "append",
        serde_json::json!({
            "delta": format!("{}🌊\r\n{}", "x".repeat(padding), "🌊".repeat(CHUNK / 4))
        }),
    );
    assert!(large.len() > CHUNK);
    let absolute_emoji = encode_header(&header()).len() + large.find('🌊').unwrap();
    assert_eq!(
        absolute_emoji,
        CHUNK - 1,
        "UTF-8 must straddle the scan boundary"
    );
    let tail = cold_frame_row(2, "append", serde_json::json!([null, {"tail": "🌊"}]));
    // Fresh monotone sequences + recognizer success force load_streaming's
    // metadata-only branch, unlike tests that only call the recognizer.
    for line in [&large, &tail] {
        assert!(crate::session::jsonl::cold_decode::parse_cold_frame(line).is_some());
    }
    let mut eager = SessionState::new();
    for line in [&large, &tail] {
        eager.apply_mutation(parse_mutation(line).unwrap()).unwrap();
    }
    for (index, (suffix, policy)) in [
        ("\n", ReadPolicy::ResourceTransitionStrict),
        ("", ReadPolicy::Restore),
        ("\n{\"kind\":", ReadPolicy::Restore),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("large-frame-{index}.jsonl");
        let bytes = format!(
            "{}{}\r\n{}{}",
            encode_header(&header()),
            large,
            tail,
            suffix
        );
        fs.write_file(&path, FileContent::Text(bytes.clone()), None)
            .await
            .unwrap();
        let (result, reader) = load_frame_history(fs.clone(), &path, policy).await;
        let storage = result.unwrap();
        if index == 0 {
            assert_eq!(
                reader.calls.load(Ordering::Relaxed),
                bytes.len().div_ceil(CHUNK)
            );
        }
        assert!(reader
            .ranges
            .lock()
            .unwrap()
            .iter()
            .all(|(_, length)| *length <= CHUNK));
        let before = reader.calls.load(Ordering::Relaxed);
        storage.facts().await.unwrap();
        assert_eq!(storage.state.lock().await.sequence(), 2);
        assert_eq!(
            reader.calls.load(Ordering::Relaxed),
            before,
            "metadata must not hydrate frame bodies"
        );
        let query = crate::session::types::RecordQuery::default();
        assert!(
            storage
                .state
                .lock()
                .await
                .find_records(&query)
                .unwrap()
                .is_empty(),
            "fresh frames must remain outside the resident reducer"
        );
        assert_eq!(
            storage.find_records(&query).await.unwrap(),
            eager.find_records(&query).unwrap()
        );
        assert_eq!(
            storage.find_records(&query).await.unwrap(),
            eager.find_records(&query).unwrap()
        );
        let repaired = fs.read_text_file(&path, None).await.unwrap();
        assert!(repaired.ends_with('\n'));
        assert_eq!(repaired.lines().count(), 3);
    }
}

#[tokio::test]
async fn raw_value_clear_rows_keep_canonical_tail_and_policy_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let append = cold_frame_row(1, "append", serde_json::json!({"delta": "kept"}));
    let bad_clear = cold_frame_row(
        2,
        "clear_run",
        serde_json::json!({"$serde_json::private::RawValue": 0}),
    );
    // This row is valid outer JSON, but canonical Value decoding rejects its
    // raw_value protocol value. The cold recognizer must therefore never
    // retire the preceding append before policy-specific canonical handling.
    assert!(crate::session::jsonl::cold_decode::parse_cold_frame(&bad_clear).is_none());
    assert!(parse_mutation(&bad_clear).is_err());

    let make_history = |middle: bool| {
        if middle {
            format!(
                "{}{}\n{}\n{}\n",
                encode_header(&header()),
                append,
                bad_clear,
                cold_frame_row(3, "append", serde_json::json!({"delta": "after"}))
            )
        } else {
            format!("{}{}\n{}\n", encode_header(&header()), append, bad_clear)
        }
    };

    // A canonical syntax failure at the physical tail is recoverable only for
    // Restore: the repaired source contains the valid prefix and sequence 1.
    let tail = make_history(false);
    fs.write_file("raw-tail.jsonl", FileContent::Text(tail), None)
        .await
        .unwrap();
    let (restored, _) = load_frame_history(fs.clone(), "raw-tail.jsonl", ReadPolicy::Restore).await;
    let restored = restored.unwrap();
    assert_eq!(restored.state.lock().await.sequence(), 1);
    assert_eq!(
        fs.read_text_file("raw-tail.jsonl", None).await.unwrap(),
        format!("{}{}\n", encode_header(&header()), append)
    );

    // FactsOnly has the same acceptance boundary but cannot publish repair;
    // the prior append remains accepted and the bad tail remains untouched.
    let facts_tail = make_history(false);
    fs.write_file(
        "raw-facts-tail.jsonl",
        FileContent::Text(facts_tail.clone()),
        None,
    )
    .await
    .unwrap();
    let (facts, _) =
        load_frame_history(fs.clone(), "raw-facts-tail.jsonl", ReadPolicy::FactsOnly).await;
    assert_eq!(facts.unwrap().state.lock().await.sequence(), 1);
    assert_eq!(
        fs.read_text_file("raw-facts-tail.jsonl", None)
            .await
            .unwrap(),
        facts_tail
    );

    // Middle corruption is never a torn tail, and strict mode rejects the
    // same canonical error even when it is physically last.
    for (path, history, policy) in [
        (
            "raw-middle-restore.jsonl",
            make_history(true),
            ReadPolicy::Restore,
        ),
        (
            "raw-middle-facts.jsonl",
            make_history(true),
            ReadPolicy::FactsOnly,
        ),
        (
            "raw-tail-strict.jsonl",
            make_history(false),
            ReadPolicy::ResourceTransitionStrict,
        ),
    ] {
        fs.write_file(path, FileContent::Text(history), None)
            .await
            .unwrap();
        let (result, _) = load_frame_history(fs.clone(), path, policy).await;
        assert!(
            result.is_err(),
            "canonical raw_value error must remain visible: {path}"
        );
    }
}
