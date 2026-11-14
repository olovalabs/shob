//! Dedicated bounded context-usage backend regressions (no real session files).
use super::*;
use crate::compaction::{estimate_context_tokens, ContextUsageEstimate};
use crate::session::{jsonl::encode_mutation, state::SessionState};
use async_trait::async_trait;
use rpi_agent::message::{AgentMessage, CustomMessage};
use rpi_ai::types::{Api, AssistantMessage, StopReason, Usage, UserContent, UserMessage};
use rpi_tools::FileError;
use std::sync::Mutex;

#[derive(Default)]
struct ObservedReader {
    bytes: Mutex<Vec<u8>>,
    ranges: Mutex<Vec<(u64, usize)>>,
}
#[async_trait]
impl ReadFile for ObservedReader {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        panic!("context usage must only read selected ranges")
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.ranges.lock().unwrap().push((offset, length));
        Ok(self.bytes.lock().unwrap()[offset as usize..offset as usize + length].to_vec())
    }
}
#[derive(Default)]
struct Fixture {
    cold: ResidentState,
    eager: SessionState,
    reader: Arc<ObservedReader>,
    rows: Vec<(usize, usize)>,
}
impl Fixture {
    fn push(&mut self, mut entry: Entry, lane: &str, resident: bool) {
        let seq = self.cold.sequence() + 1;
        let parent_id = self.cold.require_lane(lane).unwrap();
        *entry.base_mut() = EntryBase {
            entry_type: entry.entry_type().into(),
            id: format!("e{seq}"),
            seq,
            parent_id,
            timestamp: 1,
        };
        let mutation = SessionMutation::Entry {
            seq,
            timestamp: 1,
            lane: Some(lane.into()),
            entry,
        };
        self.eager.apply_mutation(mutation.clone()).unwrap();
        if resident {
            self.cold.apply_mutation(mutation).unwrap();
        } else {
            let raw = encode_mutation(&mutation);
            let offset = self.reader.bytes.lock().unwrap().len();
            self.reader
                .bytes
                .lock()
                .unwrap()
                .extend_from_slice(raw.as_bytes());
            self.rows.push((offset, raw.len()));
            let reader: Arc<dyn ReadFile> = self.reader.clone();
            self.cold
                .apply_loaded(mutation, &reader, offset as u64, raw.as_bytes())
                .unwrap();
        }
    }
    fn lane(&mut self, lane: &str, leaf: Option<&str>) {
        let mutation = SessionMutation::Lane {
            seq: self.cold.sequence() + 1,
            lane: lane.into(),
            leaf_id: leaf.map(str::to_owned),
        };
        self.cold.apply_mutation(mutation.clone()).unwrap();
        self.eager.apply_mutation(mutation).unwrap();
    }
    fn expected(&self, lane: &str) -> ContextUsageEstimate {
        let leaf = self.cold.require_lane(lane).unwrap();
        let messages = leaf
            .map(|id| {
                self.eager
                    .context_on_branch(&BranchBounds::default(), &id)
                    .unwrap()
                    .messages
            })
            .unwrap_or_default();
        estimate_context_tokens(&messages)
    }
    async fn compare(&self, lane: &str) -> ContextUsageEstimate {
        let result = self
            .cold
            .context_usage_plan(lane)
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(result, self.expected(lane));
        result
    }
}
fn base(kind: &str) -> EntryBase {
    EntryBase {
        entry_type: kind.into(),
        id: String::new(),
        seq: 0,
        parent_id: None,
        timestamp: 1,
    }
}
fn message(message: AgentMessage) -> Entry {
    Entry::Message(MessageEntry {
        base: base("message"),
        message,
        terminate: None,
    })
}
fn user(text: &str) -> Entry {
    message(AgentMessage::User(UserMessage::new(
        UserContent::Text(text.into()),
        1,
    )))
}
fn assistant(tokens: i64, reason: StopReason) -> AgentMessage {
    let mut assistant = AssistantMessage::empty(Api::Faux, "faux", "faux", 1);
    assistant.usage = Usage {
        input: tokens,
        ..Usage::zero()
    };
    assistant.stop_reason = reason;
    assistant
        .content
        .push(rpi_ai::types::Content::text("assistant body"));
    AgentMessage::Assistant(Box::new(assistant))
}
fn summary(text: &str) -> Entry {
    Entry::BranchSummary(BranchSummaryEntry {
        base: base("branch_summary"),
        summary: text.into(),
        from_id: "source".into(),
        details: None,
        usage: Some(Usage {
            input: 999_999,
            ..Usage::zero()
        }),
    })
}
fn compaction(tail: Vec<AgentMessage>) -> Entry {
    Entry::Compaction(CompactionEntry {
        base: base("compaction"),
        summary: "summary with unicode 🌊".into(),
        tokens_before: 500,
        retained_tail: tail,
        details: None,
        // This is NOT a context assistant usage.
        usage: Some(Usage {
            input: 888_888,
            ..Usage::zero()
        }),
    })
}

#[tokio::test]
async fn context_usage_matches_canonical_all_fields_and_exact_projection_offset() {
    let mut f = Fixture::default();
    f.push(user("before boundary"), "main", false);
    f.push(
        compaction(vec![
            assistant(50, StopReason::Stop),
            assistant(70, StopReason::Deferred),
            AgentMessage::User(UserMessage::new(UserContent::Text("tail".into()), 1)),
        ]),
        "main",
        false,
    );
    f.push(summary("branch"), "main", false);
    f.push(summary(""), "main", false);
    f.push(
        Entry::Custom(CustomEntry {
            base: base("custom"),
            custom_type: "unregistered".into(),
            data: None,
        }),
        "main",
        false,
    );
    f.push(message(assistant(400, StopReason::Deferred)), "main", false);
    f.push(message(assistant(500, StopReason::Error)), "main", false);
    f.push(message(assistant(600, StopReason::Aborted)), "main", false);
    f.push(user("before new anchor"), "main", false);
    let before = f.compare("main").await;
    // Compaction tails retain Deferred verbatim; the canonical estimator accepts
    // its usage, unlike an ordinary Deferred entry which projects no messages.
    assert_eq!(before.usage_tokens, 70);
    assert_eq!(before.last_usage_index, Some(2));
    f.push(message(assistant(1000, StopReason::Stop)), "main", false);
    f.push(message(assistant(0, StopReason::Stop)), "main", true);
    f.push(
        message(AgentMessage::Custom(CustomMessage::new(
            "note",
            vec![rpi_ai::types::Content::text("custom message")],
            serde_json::Value::Null,
            1,
        ))),
        "main",
        false,
    );
    f.push(user("trailing🌊"), "main", false);
    // The anchor offset must be derived without reading even a corrupt
    // compaction body (including its Deferred retained-tail messages).
    let compaction_offset = f.rows[1].0;
    f.reader.bytes.lock().unwrap()[compaction_offset] = b'!';
    let after = f.compare("main").await;
    assert_eq!(after.usage_tokens, 1000);
    assert_eq!(after.last_usage_index, Some(8));
    assert!(after.trailing_tokens > 0);
    // A second ordinary usage replaces all previous usage and trailing tokens.
    f.push(message(assistant(1200, StopReason::Stop)), "main", false);
    assert_eq!(f.compare("main").await.last_usage_index, Some(12));
}

#[tokio::test]
async fn context_usage_skips_huge_corrupt_prefix_and_invisible_rows_without_cache() {
    let mut f = Fixture::default();
    f.push(user(&"x".repeat(1024 * 1024)), "main", false);
    f.push(summary("counts toward index"), "main", false);
    f.push(message(assistant(100, StopReason::Deferred)), "main", false);
    f.push(message(assistant(200, StopReason::Stop)), "main", false);
    f.push(user("tail"), "main", false);
    let anchor_offset = f.rows[3].0;
    f.reader.bytes.lock().unwrap()[0] = b'!';
    for iteration in 1..=2 {
        let result = f.compare("main").await;
        assert_eq!(result.last_usage_index, Some(2));
        let reads = f.reader.ranges.lock().unwrap();
        assert_eq!(reads.len(), iteration);
        assert!(reads
            .iter()
            .all(|&(offset, _)| offset as usize >= anchor_offset));
    }
    let tail_offset = f.rows[4].0;
    f.reader.bytes.lock().unwrap()[tail_offset] = b'!';
    let error = f
        .cold
        .context_usage_plan("main")
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert!(error.message.contains("source changed"));
}

#[tokio::test]
async fn context_usage_no_usage_has_bounded_batches_and_standalone_large_rows() {
    for size in [4, 20_000] {
        let mut f = Fixture::default();
        for _ in 0..50 {
            f.push(user(&"x".repeat(size)), "main", false);
        }
        let first = f.compare("main").await;
        assert_eq!(first.last_usage_index, None);
        let ranges = f.reader.ranges.lock().unwrap().clone();
        assert!(ranges.len() >= 4);
        for &(offset, length) in &ranges {
            assert!(length <= 256 * 1024);
            let count = f
                .rows
                .iter()
                .filter(|&&(start, _)| start >= offset as usize && start < offset as usize + length)
                .count();
            assert!(count <= 16, "read hydrated {count} rows");
        }
        f.compare("main").await;
        assert_eq!(f.reader.ranges.lock().unwrap().len(), ranges.len() * 2);
    }
    let mut f = Fixture::default();
    f.push(user("cold"), "main", false);
    f.push(user(&"R".repeat(400_000)), "main", true);
    f.push(user(&"L".repeat(400_000)), "main", false);
    f.push(user("last"), "main", false);
    f.compare("main").await;
    let ranges = f.reader.ranges.lock().unwrap();
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[1], (f.rows[1].0 as u64, f.rows[1].1));
    assert!(ranges[1].1 > 256 * 1024);
}

#[tokio::test]
async fn context_usage_latest_compaction_is_boundary_not_usage() {
    let mut f = Fixture::default();
    f.push(
        compaction(vec![assistant(9000, StopReason::Stop)]),
        "main",
        false,
    );
    f.push(user("discarded"), "main", false);
    f.push(
        compaction(vec![assistant(2000, StopReason::Error)]),
        "main",
        false,
    );
    f.push(user("visible"), "main", false);
    f.reader.bytes.lock().unwrap()[0] = b'!';
    let result = f.compare("main").await;
    assert_eq!(result.usage_tokens, 0);
    assert_eq!(result.last_usage_index, None);
    assert!(f
        .reader
        .ranges
        .lock()
        .unwrap()
        .iter()
        .all(|&(offset, _)| offset >= f.rows[2].0 as u64));
}

#[tokio::test]
async fn context_usage_plans_pin_lane_and_original_generation_across_append_and_replace() {
    let mut f = Fixture::default();
    f.push(user("root"), "main", false);
    f.lane("side", Some("e1"));
    f.push(message(assistant(100, StopReason::Stop)), "main", false);
    f.push(message(assistant(200, StopReason::Stop)), "side", false);
    let main = f.cold.context_usage_plan("main").unwrap();
    let side = f.cold.context_usage_plan("side").unwrap();
    let expected_main = f.expected("main");
    let expected_side = f.expected("side");
    f.push(message(assistant(300, StopReason::Stop)), "main", false);
    assert_eq!(f.compare("main").await.usage_tokens, 300);
    assert_eq!(f.compare("side").await.usage_tokens, 200);
    // Replace state with an unrelated generation whose file starts at offset 0.
    let mut replacement = Fixture::default();
    replacement.push(user("replacement"), "main", false);
    f.cold = replacement.cold;
    assert_eq!(main.read().await.unwrap(), expected_main);
    assert_eq!(side.read().await.unwrap(), expected_side);
    assert!(replacement.reader.ranges.lock().unwrap().is_empty());
}

#[tokio::test]
async fn context_usage_empty_and_invalid_lane_do_not_read() {
    let f = Fixture::default();
    assert_eq!(f.compare("main").await, ContextUsageEstimate::default());
    assert!(f.cold.context_usage_plan("missing").is_err());
    assert!(f.reader.ranges.lock().unwrap().is_empty());
}

#[tokio::test]
async fn context_usage_legacy_backend_none_survives_session_and_lane_forwarding() {
    use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    use crate::session::session::Session;
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "test-only".into(),
            created_at: 1,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    storage.create_lane("side", None).await.unwrap();
    assert_eq!(storage.context_usage("main").await.unwrap(), None);
    let session = Session::new(storage, None);
    assert_eq!(session.context_usage().await.unwrap(), None);
    assert_eq!(session.view("main").context_usage().await.unwrap(), None);
    assert_eq!(session.view("side").context_usage().await.unwrap(), None);
}

#[tokio::test]
async fn context_usage_matches_canonical_usage_components_stop_reasons_and_negative_values() {
    for reason in [
        StopReason::Pending,
        StopReason::Stop,
        StopReason::Length,
        StopReason::ToolUse,
        StopReason::Error,
        StopReason::Aborted,
        StopReason::Deferred,
    ] {
        for input in [-100, -7, 0, 23] {
            for retained in [false, true] {
                let mut f = Fixture::default();
                let mut candidate = assistant(input, reason);
                if let AgentMessage::Assistant(a) = &mut candidate {
                    a.usage.cache_read = 5;
                    a.usage.cache_write = 2;
                    a.usage.output = 4;
                    // Context usage must use components, not this unrelated total.
                    a.usage.total_tokens = 999_999;
                    a.usage.reasoning = Some(3);
                    a.usage.cache_write_1h = Some(1);
                }
                f.push(user("root"), "main", false);
                if retained {
                    f.push(
                        compaction(vec![assistant(50, StopReason::Stop), candidate]),
                        "main",
                        false,
                    );
                } else {
                    f.push(message(assistant(50, StopReason::Stop)), "main", false);
                    f.push(message(candidate), "main", false);
                }
                f.push(user("tail🌊"), "main", false);
                f.compare("main").await;
            }
        }
    }
}
