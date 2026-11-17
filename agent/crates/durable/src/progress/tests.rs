use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug, Serialize, Deserialize)]
struct Frame {
    id: String,
    lane: String,
    run: String,
    stream: u64,
    seq: u64,
    timestamp: i64,
    payload: Option<String>,
    #[serde(skip)]
    clones: Arc<AtomicUsize>,
    // Test-only encoding: 0 append, 1 clear stream, 2 clear run, 3 binding.
    op: u8,
}
impl Clone for Frame {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            id: self.id.clone(),
            lane: self.lane.clone(),
            run: self.run.clone(),
            stream: self.stream,
            seq: self.seq,
            timestamp: self.timestamp,
            payload: self.payload.clone(),
            clones: self.clones.clone(),
            op: self.op,
        }
    }
}
impl ProgressRecord for Frame {
    fn id(&self) -> &str {
        &self.id
    }
    fn lane(&self) -> &str {
        &self.lane
    }
    fn run_id(&self) -> &str {
        &self.run
    }
    fn stream_index(&self) -> u64 {
        self.stream
    }
    fn sequence(&self) -> u64 {
        self.seq
    }
    fn op(&self) -> ProgressOp {
        match self.op {
            0 => ProgressOp::Append,
            1 => ProgressOp::ClearStream,
            2 => ProgressOp::ClearRun,
            _ => ProgressOp::CommitStream,
        }
    }
    fn has_payload(&self) -> bool {
        self.payload.is_some()
    }
    fn stamp(&mut self, sequence: u64, timestamp: i64) {
        self.seq = sequence;
        self.timestamp = timestamp;
    }
}
fn frame(id: &str, lane: &str, run: &str, stream: u64, op: u8) -> Frame {
    Frame {
        id: id.into(),
        lane: lane.into(),
        run: run.into(),
        stream,
        op,
        seq: 0,
        timestamp: 0,
        payload: (op == 0).then(|| "payload🌊".into()),
        clones: Arc::new(AtomicUsize::new(0)),
    }
}
fn append(state: &mut ProgressState<Frame>, frames: Vec<Frame>) {
    let batch = state.prepare(frames, || 17).unwrap();
    state.apply(&batch);
}
fn ids(state: &ProgressState<Frame>) -> Vec<&str> {
    state.frames.iter().map(|f| f.id.as_str()).collect()
}

#[test]
fn prepare_validates_all_before_stamping_or_mutating() {
    let mut state = ProgressState::default();
    append(&mut state, vec![frame("a", "main", "run", 0, 0)]);
    for batch in [
        vec![
            frame("b", "main", "run", 0, 0),
            frame("b", "main", "run", 1, 0),
        ],
        vec![frame("a", "main", "run", 0, 0)],
        vec![frame("", "main", "run", 0, 0)],
        vec![frame("b", "", "run", 0, 0)],
        vec![frame("b", "main", "", 0, 0)],
        vec![frame("b", "main", "run", 0, 3)],
    ] {
        let result = state.prepare(batch, || panic!("must validate before stamping"));
        assert!(matches!(result, Err(ProgressError::InvalidPayload(_))));
    }
    assert_eq!(ids(&state), ["a"]);
    assert_eq!(state.sequence, 1);
    assert_eq!(state.frames[0].timestamp, 17);
    let mut missing_payload = frame("b", "main", "run", 0, 0);
    missing_payload.payload = None;
    assert!(state.prepare(vec![missing_payload], || 0).is_err());
    let mut clear_with_payload = frame("c", "main", "run", 0, 2);
    clear_with_payload.payload = Some("not allowed".into());
    assert!(state.prepare(vec![clear_with_payload], || 0).is_err());
}

#[test]
fn sequence_overflow_never_publishes_a_partial_batch() {
    let state = ProgressState::<Frame>::at_sequence(u64::MAX - 1);
    let result = state.prepare(
        vec![frame("a", "main", "r", 0, 0), frame("b", "main", "r", 0, 0)],
        || 1,
    );
    assert!(matches!(result, Err(ProgressError::SequenceOverflow(_))));
    assert_eq!(state.sequence, u64::MAX - 1);
    assert!(state.frames.is_empty());
}

#[test]
fn stream_and_run_projection_preserves_other_scopes_and_does_not_publish() {
    let mut state = ProgressState::default();
    append(
        &mut state,
        vec![
            frame("a", "main", "r", 0, 0),
            frame("b", "main", "r", 1, 0),
            frame("c", "other", "r", 0, 0),
            frame("d", "main", "other", 0, 0),
        ],
    );
    let clear = state
        .prepare(vec![frame("clear", "main", "r", 0, 1)], || 2)
        .unwrap();
    let next = state.project(&clear);
    assert_eq!(ids(&next), ["b", "c", "d"]);
    assert_eq!(next.sequence, 5);
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
    assert_eq!(
        state.frames[0].clones.load(Ordering::Relaxed),
        1,
        "retired payload not cloned into candidate"
    );
    let clear = next
        .prepare(vec![frame("clear-run", "main", "r", 0, 2)], || 3)
        .unwrap();
    let projected = next.project(&clear);
    assert_eq!(ids(&projected), ["c", "d"]);
    let mut applied = next;
    applied.apply(&clear);
    assert_eq!(
        serde_json::to_value(applied).unwrap(),
        serde_json::to_value(projected).unwrap()
    );
}

#[test]
fn completed_projection_clones_only_survivors_and_keeps_sequence() {
    let mut state = ProgressState::default();
    append(
        &mut state,
        vec![
            frame("a", "main", "r", 0, 0),
            frame("b", "other", "r", 0, 0),
        ],
    );
    let empty = CompletedProgressRuns::new();
    assert!(state.project_completed(&empty).is_none());
    assert_eq!(state.frames[0].clones.load(Ordering::Relaxed), 1);
    let completed = [("main".into(), ["r".into()].into())].into();
    let next = state.project_completed(&completed).unwrap();
    assert_eq!(ids(&next), ["b"]);
    assert_eq!(next.sequence, 2);
    assert_eq!(state.frames[0].clones.load(Ordering::Relaxed), 1);
    assert_eq!(state.frames[1].clones.load(Ordering::Relaxed), 2);
    assert_eq!(state.prune_completed(&completed), 1);
    assert_eq!(ids(&state), ids(&next));
    assert_eq!(state.sequence, 2);
    // Retired identities no longer consume the active checkpoint's ID set.
    assert!(state
        .prepare(vec![frame("a", "main", "r", 0, 0)], || 1)
        .is_ok());
}

#[test]
fn checkpoint_roundtrip_rebuilds_identity_validation_without_serializing_index() {
    let mut state = ProgressState::default();
    append(&mut state, vec![frame("a", "main", "r", 0, 0)]);
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(value["sequence"], 1);
    let mut restored: ProgressState<Frame> = serde_json::from_value(value).unwrap();
    restored.validate().unwrap();
    assert!(restored
        .prepare(vec![frame("a", "main", "r", 0, 0)], || 0)
        .is_err());
    let batch = restored
        .prepare(vec![frame("b", "main", "r", 0, 0)], || 18)
        .unwrap();
    restored.apply(&batch);
    assert_eq!(restored.read("r").len(), 2);
    assert!(restored.read("absent").is_empty());
}

#[test]
fn checkpoint_rejects_unknown_fields_and_invalid_sequence_identity_or_op() {
    for defect in [
        "duplicate",
        "zero",
        "backwards",
        "ahead",
        "identity",
        "clear",
        "payload",
    ] {
        let mut state = ProgressState::default();
        append(
            &mut state,
            vec![frame("a", "main", "r", 0, 0), frame("b", "main", "r", 1, 0)],
        );
        match defect {
            "duplicate" => state.frames[1].id = "a".into(),
            "zero" => state.frames[0].seq = 0,
            "backwards" => state.frames[1].seq = 1,
            "ahead" => state.frames[1].seq = 3,
            "identity" => state.frames[1].run.clear(),
            "clear" => state.frames[1].op = 1,
            "payload" => state.frames[1].payload = None,
            _ => unreachable!(),
        }
        assert!(
            matches!(state.validate(), Err(ProgressError::InvalidCheckpoint(_))),
            "{defect}"
        );
    }
    assert!(serde_json::from_value::<ProgressState<Frame>>(
        serde_json::json!({"sequence":0,"frames":[],"ids":[]})
    )
    .is_err());
}

#[test]
fn batch_replay_and_project_match_with_interleaved_clears() {
    let mut state = ProgressState::default();
    append(&mut state, vec![frame("a", "main", "r", 0, 0)]);
    let batch = state
        .prepare(
            vec![
                frame("b", "main", "r", 0, 0),
                frame("c", "main", "r", 0, 1),
                frame("d", "main", "r", 0, 0),
                frame("e", "other", "r", 0, 0),
            ],
            || 2,
        )
        .unwrap();
    let candidate = state.project(&batch);
    state.apply(&batch);
    assert_eq!(ids(&state), ["d", "e"]);
    assert_eq!(
        serde_json::to_value(state).unwrap(),
        serde_json::to_value(candidate).unwrap()
    );
}
