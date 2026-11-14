use base64::{engine::general_purpose::STANDARD, Engine};
use rpi_harness::{
    error::{SessionError, SessionResult},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, InMemorySessionStorage, Session, SystemClock,
    },
    tool_program_body::*,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn memory(name: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: name.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
async fn begin(s: &Session) -> String {
    s.append_custom_entry(
        "rpi.tool_program.child.begin.v1",
        Some(json!({"tool":"test"})),
    )
    .await
    .unwrap()
}
fn query(d: &BodyDescriptor) -> BodyPageQuery {
    BodyPageQuery {
        descriptor_id: d.descriptor_id.clone(),
        leaf_id: None,
        cursor: None,
        limit: 8,
        max_bytes: MAX_PAGE_BYTES,
    }
}
async fn roundtrip(s: &Session, d: &BodyDescriptor) -> Vec<u8> {
    let mut q = query(d);
    let mut bytes = Vec::new();
    loop {
        let p = read_body_page(s, "main", &q).await.unwrap();
        assert!(serde_json::to_vec(&p).unwrap().len() <= MAX_RESPONSE_BYTES);
        assert!(p.byte_length <= MAX_PAGE_BYTES);
        assert_eq!(p.byte_offset, bytes.len());
        for c in p.chunks {
            assert_eq!(c.byte_offset, bytes.len());
            let decoded = STANDARD.decode(c.data).unwrap();
            assert_eq!(decoded.len(), c.byte_length);
            assert_eq!(format!("{:x}", Sha256::digest(&decoded)), c.sha256);
            bytes.extend(decoded);
        }
        q.cursor = p.next_cursor;
        if q.cursor.is_none() {
            break;
        }
    }
    assert_eq!(bytes.len(), d.byte_length);
    assert_eq!(format!("{:x}", Sha256::digest(&bytes)), d.sha256);
    bytes
}
async fn custom(s: &Session, id: String, typ: &str, data: Value) {
    s.append_entry(
        ProvisionedEntry {
            id,
            kind: ProvisionedKind::Custom {
                custom_type: typ.into(),
                data: Some(data),
            },
        },
        "main",
    )
    .await
    .unwrap();
}
fn fixture_descriptor(child: &str, bytes: usize) -> BodyDescriptor {
    let body = uuid::Uuid::new_v4().to_string();
    BodyDescriptor {
        descriptor_id: format!("tpbody-{body}-commit"),
        version: 1,
        body_id: body,
        child_id: child.into(),
        kind: BodyKind::Final,
        codec: "native-json-v1".into(),
        encoding: "base64".into(),
        byte_length: bytes,
        chunk_bytes: CHUNK_BYTES,
        chunks: bytes.div_ceil(CHUNK_BYTES),
        sha256: "0".repeat(64),
    }
}

#[tokio::test]
async fn memory_large_signed_image_utf8_roundtrip_and_descriptor_last() {
    let s = memory("memory");
    let child = begin(&s).await;
    let value = json!({"text":"é🦀中".repeat(180_000),"image":{"data":"aGVsbG8=".repeat(150_000),"mimeType":"image/png"},"signature":"signed-original", "thinking":"exact"});
    let expected = serde_json::to_vec(&value).unwrap();
    let d = append_body(&s, "main", &child, BodyKind::Final, &value, MAX_BODY_BYTES)
        .await
        .unwrap();
    assert!(d.byte_length > 1024 * 1024);
    drop(value); // guest/result-ref lifetime is not the archival lifetime
    assert_eq!(roundtrip(&s, &d).await, expected);
    let entries = s
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.last().unwrap().id(), d.descriptor_id);
    assert_eq!(entries.len(), d.chunks + 2);
    for (index, e) in entries[1..entries.len() - 1].iter().enumerate() {
        assert_eq!(e.id(), format!("tpbody-{}-chunk-{index}", d.body_id));
        assert!(serde_json::to_vec(e).unwrap().len() <= MAX_ENTRY_BYTES);
    }
    // Place a multibyte character across the decoded chunk boundary.
    let text = format!("{}🦀", "x".repeat(CHUNK_BYTES - 2));
    let d = append_body(&s, "main", &child, BodyKind::Update, &text, MAX_BODY_BYTES)
        .await
        .unwrap();
    let p = read_body_page(&s, "main", &query(&d)).await.unwrap();
    assert!(std::str::from_utf8(&STANDARD.decode(&p.chunks[0].data).unwrap()).is_err());
    assert_eq!(roundtrip(&s, &d).await, serde_json::to_vec(&text).unwrap());
}

#[tokio::test]
async fn quota_and_serialization_fail_before_any_append() {
    let s = memory("quota");
    let child = begin(&s).await;
    let before = s.storage().history_revision().await.unwrap();
    for quota in [0, 1, MAX_BODY_BYTES + 1] {
        assert!(
            append_body(&s, "main", &child, BodyKind::Requested, &"too large", quota)
                .await
                .is_err()
        );
    }
    assert!(
        append_body(&s, "main", "arbitrary/path", BodyKind::Final, &true, 100)
            .await
            .is_err()
    );
    assert!(append_body(
        &s,
        "main",
        &child,
        BodyKind::Final,
        &"x".repeat(MAX_BODY_BYTES),
        MAX_BODY_BYTES
    )
    .await
    .is_err());
    struct Broken;
    impl serde::Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("no"))
        }
    }
    assert!(
        append_body(&s, "main", &child, BodyKind::Final, &Broken, 100)
            .await
            .is_err()
    );
    assert_eq!(s.storage().history_revision().await.unwrap(), before);
    let d = append_body(&s, "main", &child, BodyKind::Prepared, &0, 1)
        .await
        .unwrap();
    assert_eq!(roundtrip(&s, &d).await, b"0");
}

#[tokio::test]
async fn strict_pages_and_cursor_source_binding() {
    let s = memory("cursors");
    let child = begin(&s).await;
    let d = append_body(
        &s,
        "main",
        &child,
        BodyKind::Final,
        &"x".repeat(CHUNK_BYTES * 4),
        MAX_BODY_BYTES,
    )
    .await
    .unwrap();
    let mut q = query(&d);
    q.limit = 1;
    let p = read_body_page(&s, "main", &q).await.unwrap();
    let cursor = p.next_cursor.unwrap();
    for (count, cap) in [
        (0, 1),
        (9, 1),
        (1, 0),
        (1, MAX_PAGE_BYTES + 1),
        (1, CHUNK_BYTES - 1),
    ] {
        let mut bad = query(&d);
        bad.limit = count;
        bad.max_bytes = cap;
        assert!(read_body_page(&s, "main", &bad).await.is_err());
    }
    for field in 0..5 {
        let mut bad = q.clone();
        let mut c = cursor.clone();
        match field {
            0 => c.session_id = "foreign".into(),
            1 => c.lane = "other".into(),
            2 => c.leaf_id = child.clone(),
            3 => c.descriptor_id = "foreign".into(),
            _ => c.next_index = usize::MAX,
        }
        bad.cursor = Some(c);
        assert!(read_body_page(&s, "main", &bad).await.is_err());
    }
    let other = begin(&s).await;
    let mut pinned = q.clone();
    pinned.cursor = Some(cursor.clone());
    assert!(read_body_page(&s, "main", &pinned).await.is_ok()); // normal leaf advance
    pinned.leaf_id = Some(other);
    assert!(read_body_page(&s, "main", &pinned).await.is_err());
    s.create_lane("foreign", Some(&child)).await.unwrap();
    s.view("foreign")
        .append_custom_entry("foreign", None)
        .await
        .unwrap();
    let foreign = s
        .get_lanes()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.lane == "foreign")
        .unwrap()
        .leaf_id
        .unwrap();
    let mut bad = query(&d);
    bad.leaf_id = Some(foreign);
    assert!(read_body_page(&s, "main", &bad).await.is_err());
    s.move_lane("main", Some(&child)).await.unwrap();
    assert!(read_body_page(&s, "main", &pinned).await.is_err());
}

#[tokio::test]
async fn missing_commit_missing_chunk_and_forged_shapes_fail_closed() {
    let s = memory("invalid");
    let child = begin(&s).await;
    let d = fixture_descriptor(&child, 1);
    assert!(read_body_page(&s, "main", &query(&d)).await.is_err());
    custom(
        &s,
        d.descriptor_id.clone(),
        COMMIT_CUSTOM_TYPE,
        serde_json::to_value(&d).unwrap(),
    )
    .await;
    assert!(read_body_page(&s, "main", &query(&d)).await.is_err()); // no chunk, no repair
                                                                    // A chunk appended AFTER the descriptor must not repair the missing body.
    custom(&s,format!("tpbody-{}-chunk-0",d.body_id),CHUNK_CUSTOM_TYPE,json!({"bodyId":d.body_id,"childId":child,"index":0,"data":"MA==","sha256":format!("{:x}",Sha256::digest(b"0"))})).await;
    assert!(read_body_page(&s, "main", &query(&d)).await.is_err());
    for field in [
        "unknown",
        "chunks",
        "chunkBytes",
        "byteLength",
        "sha256",
        "bodyId",
        "childId",
        "descriptorId",
        "codec",
        "version",
    ] {
        let d = fixture_descriptor(&child, 1);
        let mut v = serde_json::to_value(&d).unwrap();
        v[field] = match field {
            "chunks" => json!(MAX_CHUNKS + 1),
            "chunkBytes" => json!(CHUNK_BYTES + 1),
            "byteLength" => json!(MAX_BODY_BYTES + 1),
            "version" => json!(2),
            _ => json!("forged"),
        };
        custom(&s, d.descriptor_id.clone(), COMMIT_CUSTOM_TYPE, v).await;
        assert!(
            read_body_page(&s, "main", &query(&d)).await.is_err(),
            "{field}"
        );
    }
}

#[tokio::test]
async fn corrupt_chunks_strict_dto_and_oversized_entries_rejected() {
    for flaw in [
        "hash", "data", "index", "childId", "bodyId", "unknown", "oversize",
    ] {
        let s = memory(flaw);
        let child = begin(&s).await;
        let d = fixture_descriptor(&child, 1);
        let mut c = json!({"bodyId":d.body_id,"childId":child,"index":0,"data":"MA==","sha256":format!("{:x}",Sha256::digest(b"0"))});
        match flaw {
            "hash" => c["sha256"] = json!("a".repeat(64)),
            "data" => c["data"] = json!("!!!!"),
            "index" => c["index"] = json!(1),
            "oversize" => c["data"] = json!("x".repeat(MAX_ENTRY_BYTES)),
            key => c[key] = json!("bad"),
        }
        custom(
            &s,
            format!("tpbody-{}-chunk-0", d.body_id),
            CHUNK_CUSTOM_TYPE,
            c,
        )
        .await;
        custom(
            &s,
            d.descriptor_id.clone(),
            COMMIT_CUSTOM_TYPE,
            serde_json::to_value(&d).unwrap(),
        )
        .await;
        assert!(
            read_body_page(&s, "main", &query(&d)).await.is_err(),
            "{flaw}"
        );
    }
}

#[tokio::test]
async fn real_cold_jsonl_reopen_selected_body_only_and_prefix_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("body.jsonl")
        .to_string_lossy()
        .replace('\\', "/");
    let fs: Arc<dyn rpi_tools::FileSystem> =
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let store = JsonlSessionStorage::create(
        fs.clone(),
        &path,
        JsonlV4Header::new("cold-body".into(), 1, "/".into(), None, None, None),
        clock.clone(),
        ids.clone(),
    )
    .await
    .unwrap();
    let s = Session::new(Arc::new(store), None);
    let child = begin(&s).await;
    let value = json!({"signed":"中🦀".repeat(200_000),"image":{"data":"aGVsbG8=".repeat(150_000)},"signature":"keep-exact"});
    let expected = serde_json::to_vec(&value).unwrap();
    let d = append_body(&s, "main", &child, BodyKind::Final, &value, MAX_BODY_BYTES)
        .await
        .unwrap();
    let unrelated = append_body(
        &s,
        "main",
        &child,
        BodyKind::Update,
        &"UNRELATED_PAYLOAD".repeat(6000),
        MAX_BODY_BYTES,
    )
    .await
    .unwrap();
    drop(value);
    s.drain().await;
    drop(s);
    let reopened = JsonlSessionStorage::load(fs.clone(), &path, clock.clone(), ids.clone())
        .await
        .unwrap();
    let s = Session::new(Arc::new(reopened), None);
    // Mutate an unrelated cold chunk without changing line offsets. A broad
    // hydration would trip its pinned digest; selected reads must not touch it.
    let raw = std::fs::read_to_string(&path).unwrap();
    let needle = format!("tpbody-{}-chunk-0", unrelated.body_id);
    let changed = raw.replacen(&needle, &needle.replace("chunk-0", "chunk-X"), 1);
    assert_ne!(raw, changed);
    std::fs::write(&path, changed).unwrap();
    assert_eq!(roundtrip(&s, &d).await, expected);
    let unavailable = read_body_page(&s, "main", &query(&unrelated)).await;
    assert!(unavailable.is_err());
    // Restore the probe before independently reopening a copied native prefix.
    drop(s);
    std::fs::write(&path, &raw).unwrap();
    let prefix = raw
        .lines()
        .take_while(|line| !line.contains(&d.descriptor_id))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let prefix_path = dir.path().join("prefix.jsonl");
    std::fs::write(&prefix_path, prefix).unwrap();
    std::fs::copy(
        format!("{path}.progress.jsonl"),
        format!("{}.progress.jsonl", prefix_path.display()),
    )
    .unwrap();
    let prefix = JsonlSessionStorage::load(fs, prefix_path.to_str().unwrap(), clock, ids)
        .await
        .unwrap();
    let prefix = Session::new(Arc::new(prefix), None);
    assert!(read_body_page(&prefix, "main", &query(&d)).await.is_err());
}

// Deliberately small delegating probe: exact-ID reads only; nth conditional
// append can fail before storage OR lose its ACK after storage. No auto retry.
struct Probe {
    inner: Arc<dyn SessionStorage>,
    calls: AtomicUsize,
    fail: usize,
    lost_ack: bool,
    seen: Mutex<Vec<String>>,
    pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    read_pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    read_paused: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl SessionStorage for Probe {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, l: &str, a: Option<&str>) -> SessionResult<()> {
        self.inner.create_lane(l, a).await
    }
    async fn move_lane(&self, l: &str, a: Option<&str>) -> SessionResult<()> {
        self.inner.move_lane(l, a).await
    }
    async fn append_progress(
        &self,
        r: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.append_progress(r).await
    }
    async fn read_progress(&self, r: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.read_progress(r).await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.inner.prune_completed_progress().await
    }
    async fn history_revision(&self) -> SessionResult<u64> {
        self.inner.history_revision().await
    }
    async fn append_entry(&self, e: ProvisionedEntry, l: &str) -> SessionResult<Entry> {
        self.inner.append_entry(e, l).await
    }
    async fn append_entry_if_revision(
        &self,
        e: ProvisionedEntry,
        l: &str,
        r: u64,
    ) -> SessionResult<Entry> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            if let Some((ready, go)) = &self.pause {
                ready.notify_one();
                go.notified().await;
            }
        }
        if n == self.fail && !self.lost_ack {
            return Err(SessionError::storage("injected append failure"));
        }
        let result = self.inner.append_entry_if_revision(e, l, r).await?;
        if n == self.fail {
            return Err(SessionError::storage("injected lost ACK"));
        }
        Ok(result)
    }
    async fn append_record(&self, r: LaneRecord) -> SessionResult<LaneRecord> {
        self.inner.append_record(r).await
    }
    async fn get_entry(&self, _: &str) -> SessionResult<Option<Entry>> {
        panic!("body reader must use bounded branch queries")
    }
    async fn find_entries(&self, _: &EntryQuery) -> SessionResult<Vec<Entry>> {
        panic!("full history forbidden")
    }
    async fn find_entries_on_branch(
        &self,
        q: &EntryQuery,
        b: &BranchBounds,
        s: &str,
    ) -> SessionResult<Vec<Entry>> {
        assert_eq!(q.limit, Some(1));
        self.seen
            .lock()
            .unwrap()
            .push(q.entry_id.clone().expect("exact ID only"));
        if !self.read_paused.swap(true, Ordering::SeqCst) {
            if let Some((ready, go)) = &self.read_pause {
                ready.notify_one();
                go.notified().await;
            }
        }
        self.inner.find_entries_on_branch(q, b, s).await
    }
    async fn find_records(&self, q: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.inner.find_records(q).await
    }
    async fn record_page(&self, q: &RecordPageQuery) -> SessionResult<RecordPage> {
        self.inner.record_page(q).await
    }
    async fn find_open_operations(
        &self,
        l: &str,
        n: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner.find_open_operations(l, n).await
    }
    async fn get_log(&self, q: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.get_log(q).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.inner.get_name().await
    }
    async fn set_name(&self, n: Option<&str>) -> SessionResult<()> {
        self.inner.set_name(n).await
    }
    async fn get_label(&self, i: &str) -> SessionResult<Option<String>> {
        self.inner.get_label(i).await
    }
    async fn set_label(&self, i: &str, l: Option<&str>) -> SessionResult<()> {
        self.inner.set_label(i, l).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
}
fn probe(
    s: &Session,
    fail: usize,
    lost_ack: bool,
    pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> Arc<Probe> {
    Arc::new(Probe {
        inner: s.storage(),
        calls: AtomicUsize::new(0),
        fail,
        lost_ack,
        seen: Mutex::new(vec![]),
        pause,
        read_pause: None,
        read_paused: std::sync::atomic::AtomicBool::new(false),
    })
}
#[tokio::test]
async fn append_failure_descriptor_last_and_uncertain_ack_never_retried() {
    for lost_ack in [false, true] {
        for nth in 1..=4 {
            let s = memory("failure");
            let child = begin(&s).await;
            let p = probe(&s, nth, lost_ack, None);
            let writer = Session::new(p.clone(), None);
            let result = append_body(
                &writer,
                "main",
                &child,
                BodyKind::Final,
                &"x".repeat(CHUNK_BYTES * 2),
                MAX_BODY_BYTES,
            )
            .await;
            assert!(result.is_err());
            assert_eq!(p.calls.load(Ordering::SeqCst), nth);
            let commits = s
                .find_entries(&EntryQuery {
                    custom_type: Some(COMMIT_CUSTOM_TYPE.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(commits.len(), usize::from(lost_ack && nth == 4));
            let chunks = s
                .find_entries(&EntryQuery {
                    custom_type: Some(CHUNK_CUSTOM_TYPE.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(chunks.len(), (nth - 1 + usize::from(lost_ack)).min(3));
        }
    }
}
#[tokio::test]
async fn append_and_read_pin_session_across_shared_clone_switch() {
    let original = memory("owner");
    let child = begin(&original).await;
    let ready = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    let p = probe(
        &original,
        usize::MAX,
        false,
        Some((ready.clone(), go.clone())),
    );
    let shared = Session::new(p.clone(), None);
    let switch = shared.clone();
    let task = tokio::spawn(async move {
        append_body(
            &shared,
            "main",
            &child,
            BodyKind::Final,
            &"x".repeat(CHUNK_BYTES * 3),
            MAX_BODY_BYTES,
        )
        .await
    });
    ready.notified().await;
    let destination = memory("destination");
    begin(&destination).await;
    switch.set_storage(destination.storage());
    go.notify_one();
    let d = task.await.unwrap().unwrap();
    assert!(read_body_page(&switch, "main", &query(&d)).await.is_err());
    let reader = Session::new(p.clone(), None);
    p.seen.lock().unwrap().clear();
    let _ = roundtrip(&reader, &d).await;
    {
        let reads = p.seen.lock().unwrap();
        assert!(reads.iter().all(|i| i == &d.child_id
            || i == &d.descriptor_id
            || i.starts_with(&format!("tpbody-{}-chunk-", d.body_id))));
    }
    assert!(destination
        .find_entries(&EntryQuery {
            custom_type: Some(COMMIT_CUSTOM_TYPE.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reader_pins_owner_during_session_switch() {
    let source = memory("read-owner");
    let child = begin(&source).await;
    let d = append_body(&source, "main", &child, BodyKind::Final, &"pinned", 100)
        .await
        .unwrap();
    let ready = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    let mut p = Arc::try_unwrap(probe(&source, usize::MAX, false, None))
        .ok()
        .unwrap();
    p.read_pause = Some((ready.clone(), go.clone()));
    let session = Session::new(Arc::new(p), None);
    let switched = session.clone();
    let q = query(&d);
    let task = tokio::spawn(async move { read_body_page(&session, "main", &q).await });
    ready.notified().await;
    switched.set_storage(memory("foreign-read-owner").storage());
    go.notify_one();
    let page = task.await.unwrap().unwrap();
    assert_eq!(page.descriptor, d);
    assert_eq!(
        STANDARD.decode(&page.chunks[0].data).unwrap(),
        b"\"pinned\""
    );
}

#[tokio::test]
async fn maximum_body_and_last_chunk_cursor_are_bounded() {
    let s = memory("maximum");
    let child = begin(&s).await;
    let value = "x".repeat(MAX_BODY_BYTES - 2);
    let d = append_body(&s, "main", &child, BodyKind::Final, &value, MAX_BODY_BYTES)
        .await
        .unwrap();
    assert_eq!(d.byte_length, MAX_BODY_BYTES);
    assert_eq!(d.chunks, MAX_CHUNKS);
    drop(value);
    let p = read_body_page(&s, "main", &query(&d)).await.unwrap();
    assert_eq!(p.chunks.len(), 2);
    assert_eq!(p.byte_length, MAX_PAGE_BYTES);
    let mut q = query(&d);
    let mut c = p.next_cursor.unwrap();
    c.next_index = MAX_CHUNKS - 1;
    q.cursor = Some(c);
    let last = read_body_page(&s, "main", &q).await.unwrap();
    assert_eq!(last.byte_offset, MAX_BODY_BYTES - CHUNK_BYTES);
    assert!(last.next_cursor.is_none());
    assert_eq!(
        STANDARD.decode(&last.chunks[0].data).unwrap().last(),
        Some(&b'"')
    );
}

#[tokio::test]
async fn native_forks_own_copied_entries_without_source_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("source.jsonl")
        .to_string_lossy()
        .replace(char::from(92), "/");
    let fs: Arc<dyn rpi_tools::FileSystem> =
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let header = |name: &str| JsonlV4Header::new(name.into(), 1, "/".into(), None, None, None);
    let source = Arc::new(
        JsonlSessionStorage::create(
            fs.clone(),
            &path,
            header("source"),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap(),
    );
    let s = Session::new(source.clone(), None);
    s.create_lane("spare", None).await.unwrap(); // ensure destination seq is rebased
    let first = s
        .append_message(rpi_ai::UserMessage::new("root", 0).into())
        .await
        .unwrap();
    let child = begin(&s).await;
    let d = append_body(
        &s,
        "main",
        &child,
        BodyKind::Final,
        &"owned".repeat(10000),
        MAX_BODY_BYTES,
    )
    .await
    .unwrap();
    let cursor = read_body_page(
        &s,
        "main",
        &BodyPageQuery {
            limit: 1,
            ..query(&d)
        },
    )
    .await
    .unwrap()
    .next_cursor
    .unwrap();
    let source_seq = s.get_entry(&d.descriptor_id).await.unwrap().unwrap().seq();
    let prefix_path = dir
        .path()
        .join("prefix.jsonl")
        .to_string_lossy()
        .replace(char::from(92), "/");
    let prefix = source
        .fork(
            &prefix_path,
            header("prefix"),
            &ForkOptions::Branch {
                entry_id: Some(first),
                position: None,
            },
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
    let prefix = Session::new(Arc::new(prefix), None);
    assert!(read_body_page(&prefix, "main", &query(&d)).await.is_err());
    let copied_path = dir
        .path()
        .join("copied.jsonl")
        .to_string_lossy()
        .replace(char::from(92), "/");
    let copied = source
        .fork(
            &copied_path,
            header("copied"),
            &ForkOptions::Tree,
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
    let copied = Session::new(Arc::new(copied), None);
    assert_ne!(
        copied
            .get_entry(&d.descriptor_id)
            .await
            .unwrap()
            .unwrap()
            .seq(),
        source_seq
    );
    let mut foreign = query(&d);
    foreign.cursor = Some(cursor);
    assert!(read_body_page(&copied, "main", &foreign).await.is_err());
    drop(s);
    drop(source);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(format!("{path}.progress.jsonl")).unwrap();
    assert_eq!(
        roundtrip(&copied, &d).await,
        serde_json::to_vec(&"owned".repeat(10000)).unwrap()
    );
    drop(copied);
    let reopened = JsonlSessionStorage::load(fs, &copied_path, clock, ids)
        .await
        .unwrap();
    assert_eq!(
        roundtrip(&Session::new(Arc::new(reopened), None), &d).await,
        serde_json::to_vec(&"owned".repeat(10000)).unwrap()
    );
}
