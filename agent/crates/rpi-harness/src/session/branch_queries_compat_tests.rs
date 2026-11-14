//! Legacy tree implementations inherit both APIs using only bounded reads.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use super::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
use super::types::*;
use super::Session;
use crate::error::SessionResult;

struct LegacyTree {
    session: Session,
    leaf_reads: AtomicUsize,
    reads: Mutex<Vec<(usize, String)>>,
}

#[async_trait::async_trait]
impl SessionTree for LegacyTree {
    async fn get_leaf_id(&self) -> SessionResult<Option<String>> {
        self.leaf_reads.fetch_add(1, Ordering::SeqCst);
        let leaf = self.session.get_leaf_id().await?;
        // Advance after taking the snapshot, before the first payload read.
        self.session.append_custom_entry("late", None).await?;
        Ok(leaf)
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>> {
        let limit = query
            .limit
            .expect("compatibility API must bound every read");
        let start = bounds
            .start
            .clone()
            .expect("every read must pin the same leaf");
        self.reads.lock().unwrap().push((limit, start));
        self.session.find_entries_on_branch(query, bounds).await
    }
    async fn get_entry(&self, _: &str) -> SessionResult<Option<Entry>> {
        unreachable!()
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        unreachable!()
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        unreachable!()
    }
    async fn set_name(&self, _: Option<&str>) -> SessionResult<()> {
        unreachable!()
    }
    async fn get_label(&self, _: &str) -> SessionResult<Option<String>> {
        unreachable!()
    }
    async fn set_label(&self, _: &str, _: Option<&str>) -> SessionResult<()> {
        unreachable!()
    }
    async fn find_entries(&self, _: &EntryQuery) -> SessionResult<Vec<Entry>> {
        unreachable!()
    }
    async fn find_entry(&self, _: &EntryQuery) -> SessionResult<Option<Entry>> {
        unreachable!()
    }
    async fn find_entry_on_branch(
        &self,
        _: &EntryQuery,
        _: &BranchBounds,
    ) -> SessionResult<Option<Entry>> {
        unreachable!()
    }
    async fn append_message(&self, _: rpi_agent::message::AgentMessage) -> SessionResult<String> {
        unreachable!()
    }
    async fn append_custom_entry(&self, _: &str, _: Option<JsonValue>) -> SessionResult<String> {
        unreachable!()
    }
}

fn legacy_tree() -> LegacyTree {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "legacy-tree-test".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    LegacyTree {
        session: Session::new(storage, None),
        leaf_reads: AtomicUsize::new(0),
        reads: Mutex::new(Vec::new()),
    }
}

#[tokio::test]
async fn default_tree_page_is_bounded_and_pins_one_leaf() {
    let tree = legacy_tree();
    let mut original_leaf = String::new();
    for _ in 0..70 {
        original_leaf = tree
            .session
            .append_custom_entry("test", None)
            .await
            .unwrap();
    }
    let page = tree.branch_entry_page(&Default::default()).await.unwrap();
    assert_eq!(page.entries.len(), 50);
    assert_eq!(page.last_id.as_deref(), Some(original_leaf.as_str()));
    assert_eq!(page.leaf_id.as_deref(), Some(original_leaf.as_str()));
    assert!(!page.has_more_after);
    assert_ne!(
        tree.session.get_leaf_id().await.unwrap().as_deref(),
        Some(original_leaf.as_str())
    );
    assert_eq!(tree.leaf_reads.load(Ordering::SeqCst), 1);
    assert!(tree
        .reads
        .lock()
        .unwrap()
        .iter()
        .all(|(limit, start)| *limit == 1 && start == &original_leaf));
}

#[tokio::test]
async fn default_tree_overview_uses_fixed_leaf_and_bounded_batches() {
    let tree = legacy_tree();
    let mut message = rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "p", "m", 0);
    message.stop_reason = rpi_ai::types::StopReason::Stop;
    message.usage.input = 8;
    tree.session
        .append_message(rpi_agent::message::AgentMessage::Assistant(Box::new(
            message,
        )))
        .await
        .unwrap();
    let mut original_leaf = String::new();
    for _ in 0..70 {
        original_leaf = tree
            .session
            .append_custom_entry("test", None)
            .await
            .unwrap();
    }
    let overview = tree.branch_overview().await.unwrap();
    assert_eq!(overview.input, 8);
    assert_eq!(overview.model, Some(("p".into(), "m".into())));
    assert_eq!(tree.leaf_reads.load(Ordering::SeqCst), 1);
    let reads = tree.reads.lock().unwrap();
    assert!(reads.len() >= 5);
    assert!(reads
        .iter()
        .all(|(limit, start)| *limit <= 16 && start == &original_leaf));
}
