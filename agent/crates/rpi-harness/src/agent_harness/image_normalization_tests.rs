//! Deterministic coverage of harness image preparation AFTER RPC preflight.
//! Gates are per-message and test-only, so parallel tests cannot intercept one
//! another. The blocking gate is always released even if an assertion fails.
use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Content;
use std::collections::HashMap;
use std::sync::{Condvar, OnceLock, Weak};
use std::time::Duration;

#[derive(Default)]
pub(super) struct Gate {
    entered: CancellationToken,
    finished: CancellationToken,
    released: Mutex<bool>,
    changed: Condvar,
    panic_in_worker: bool,
}

fn gates() -> &'static Mutex<HashMap<String, Weak<Gate>>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Gate {
    fn register(key: &str, panic_in_worker: bool) -> Arc<Self> {
        let gate = Arc::new(Self {
            panic_in_worker,
            ..Default::default()
        });
        let mut registry = gates().lock().unwrap();
        registry.retain(|_, gate| gate.strong_count() > 0);
        assert!(registry.insert(key.into(), Arc::downgrade(&gate)).is_none());
        gate
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }

    pub(super) fn enter_worker(self: Arc<Self>) -> WorkerFinished {
        let finished = WorkerFinished(self.clone());
        self.entered.cancel();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
        drop(released);
        assert!(!self.panic_in_worker, "injected image preparation panic");
        finished
    }
}

pub(super) struct WorkerFinished(Arc<Gate>);
impl Drop for WorkerFinished {
    fn drop(&mut self) {
        self.0.finished.cancel();
    }
}
struct ReleaseOnDrop(Arc<Gate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

pub(super) fn gate_for_message(message: &AgentMessage) -> Option<Arc<Gate>> {
    let AgentMessage::User(user) = message else {
        return None;
    };
    let UserContent::Blocks(blocks) = &user.content else {
        return None;
    };
    let Some(Content::Text(text)) = blocks.first() else {
        return None;
    };
    gates().lock().unwrap().get(&text.text)?.upgrade()
}

fn image_prompt(key: &str) -> AgentMessage {
    // Valid 1x1 PNG. The gate wraps the actual harness normalizer, not RPC's
    // independently owned preflight decoder or a simulated async worker.
    let image: Content = serde_json::from_value(serde_json::json!({
        "type": "image", "mimeType": "image/png",
        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg4RD4DwABTAEc6BPG4AAAAABJRU5ErkJggg=="
    })).unwrap();
    if let Content::Image(image) = &image {
        rpi_tools::image_processing::validate_rpc_inline_image(&image.data, &image.mime_type)
            .expect("regression image must pass actual RPC preflight");
    }
    AgentMessage::User(rpi_ai::types::UserMessage::new(
        UserContent::Blocks(vec![Content::text(key), image]),
        0,
    ))
}

async fn fixture() -> (AgentHarness, Arc<FauxProvider>) {
    let provider = FauxProvider::new(FauxScript::new().with_text("next run works"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        ..Default::default()
    })
    .await
    .unwrap();
    (harness, provider)
}

async fn assert_no_effects(harness: &AgentHarness, provider: &FauxProvider) {
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(harness
        .session()
        .storage()
        .find_entries(&crate::session::types::EntryQuery::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_during_image_preparation_retains_admission_until_worker_finishes() {
    let (harness, provider) = fixture().await;
    let first = Gate::register("abort-preparation-first", false);
    let second = Gate::register("abort-preparation-second", false);
    let _release_first = ReleaseOnDrop(first.clone());
    let _release_second = ReleaseOnDrop(second.clone());
    let running = harness.clone();
    let task = tokio::spawn(async move {
        running
            .prompt_messages(vec![
                image_prompt("abort-preparation-first"),
                image_prompt("abort-preparation-second"),
            ])
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), first.entered.cancelled())
        .await
        .unwrap();

    // This used to fail with NoActiveRun: preparation preceded acquire_run.
    let aborted = harness.abort().await.unwrap();
    assert!(!task.is_finished());
    assert!(!first.finished.is_cancelled());
    {
        let inner = harness.inner.lock().unwrap();
        let active = inner.active_run.as_ref().unwrap();
        assert_eq!(active.run_id, aborted.run_id);
        assert!(active.signal.is_cancelled());
    }
    let busy = harness
        .prompt_message(image_prompt("abort-preparation-second"))
        .await;
    assert!(matches!(busy, Err(HarnessError::LaneBusy { .. })));
    assert!(!second.entered.is_cancelled());
    assert_no_effects(&harness, &provider).await;

    first.release();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("aborted during image preparation"));
    assert!(first.finished.is_cancelled());
    assert!(
        !second.entered.is_cancelled(),
        "abort must skip remaining images"
    );
    assert!(harness.inner.lock().unwrap().active_run.is_none());
    assert_no_effects(&harness, &provider).await;
    let next = harness.prompt_text("next", Vec::new()).await.unwrap();
    assert!(matches!(next.outcome, HarnessRunOutcome::Completed { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_preparation_worker_error_releases_admission_without_side_effects() {
    let (harness, provider) = fixture().await;
    let gate = Gate::register("panic-preparation", true);
    let _release = ReleaseOnDrop(gate.clone());
    let running = harness.clone();
    let task = tokio::spawn(async move {
        running
            .prompt_message(image_prompt("panic-preparation"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.cancelled())
        .await
        .unwrap();
    assert!(harness.inner.lock().unwrap().active_run.is_some());
    gate.release();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Image processing failed"));
    assert!(gate.finished.is_cancelled());
    assert!(harness.inner.lock().unwrap().active_run.is_none());
    assert_no_effects(&harness, &provider).await;
    let next = harness.prompt_text("next", Vec::new()).await.unwrap();
    assert!(matches!(next.outcome, HarnessRunOutcome::Completed { .. }));
}
