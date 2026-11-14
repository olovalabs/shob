#[tokio::test]
async fn cold_legacy_recovery_preserves_uncommitted_bindings_and_clear_markers() {
    use crate::session::jsonl::{encode_header, JsonlSessionStorage, JsonlV4Header};
    use rpi_tools::{FileSystem, OsExecutionEnv};
    use crate::session::types::SessionStorage;
    let reference = session(false);
    let recorder = FrameRecordingEmitter::new(Arc::new(CollectorEmitter::default()), reference.clone(), "main", "run");
    for event in events() { recorder.emit(event).await; }
    recorder.flush().await.unwrap();
    let frames = reference.read_progress("run").await.unwrap();
    assert!(!frames.is_empty());
    let expected = salvage_run_frames(&reference, "run").await.unwrap();
    assert_eq!(expected.len(), 1);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.jsonl");
    let header = JsonlV4Header::new("cold-recovery".into(), 1, "/".into(), None, None, None);
    std::fs::write(&path, encode_header(&header)).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let load = || JsonlSessionStorage::load(fs.clone(), path.to_str().unwrap(), Arc::new(FakeClock::new()), Arc::new(CounterIdGenerator::new()));
    let store = load().await.unwrap();
    for frame in frames { store.append_record(LaneRecord::AssistantFrame(frame)).await.unwrap(); }
    let stored = Session::new(Arc::new(store), None);
    bind_stream_entry(&stored, "main", "run", 0, "reserved").await.unwrap();
    let original_bytes = std::fs::read(&path).unwrap();
    let loaded = Session::new(Arc::new(load().await.unwrap()), None);
    assert_eq!(salvage_run_frames(&loaded, "run").await.unwrap(), expected);
    let replayed = replay_run_streams(&loaded, "run").await.unwrap();
    assert_eq!(replayed[0].entry_id.as_deref(), Some("reserved"));
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);

    // A companion may add another stream without duplicating cold legacy data.
    let upgraded = load().await.unwrap().enable_progress_under_exclusive_lock().await.unwrap();
    let mixed = Session::new(Arc::new(upgraded), None);
    let recorder = FrameRecordingEmitter::new(Arc::new(CollectorEmitter::default()), mixed.clone(), "main", "run");
    // Clear only stream 1; stream 0's interrupted legacy output still survives.
    recorder.clear_stream(1).await.unwrap();
    assert_eq!(salvage_run_frames(&mixed, "run").await.unwrap(), expected);
    mixed.storage().append_entry(ProvisionedEntry {
        id: "reserved".into(), kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new("committed", 42).into(), terminate: None,
        },
    }, "main").await.unwrap();
    assert!(salvage_run_frames(&mixed, "run").await.unwrap().is_empty());
    recorder.clear().await.unwrap();
    assert!(salvage_run_frames(&mixed, "run").await.unwrap().is_empty());
    let reopened = Session::new(Arc::new(load().await.unwrap()), None);
    assert!(salvage_run_frames(&reopened, "run").await.unwrap().is_empty());
}
