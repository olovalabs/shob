//! Dropping a queue API caller must not release the message gate before the
//! real JSONL cancellation ACK and in-memory removal complete together.
use super::*;
use crate::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use crate::session::memory::SystemClock;
use crate::session::session::DefaultIdGenerator;
use rpi_tools::env::AppendFile;
use rpi_tools::{FileContent, FileError, FileSystem};
use std::future::{poll_fn, Future};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Gate {
    used: AtomicBool,
    entered: Notify,
    release: Notify,
}
struct CancellationAppend {
    fs: Arc<dyn FileSystem>,
    path: String,
    gate: Arc<Gate>,
}
#[async_trait::async_trait]
impl AppendFile for CancellationAppend {
    async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        if String::from_utf8_lossy(bytes).contains("queue_cancelled")
            && !self.gate.used.swap(true, Ordering::SeqCst)
        {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.fs
            .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_abort_reclaiming_caller_keeps_atomic_owned_gate() {
    tokio::time::timeout(Duration::from_secs(20), probe(true, true))
        .await
        .expect("active abort must complete after releasing append barrier");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_idle_abort_reclaiming_caller_keeps_atomic_owned_gate() {
    tokio::time::timeout(Duration::from_secs(20), probe(true, false))
        .await
        .expect("idle abort must complete after releasing append barrier");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_clear_queue_caller_keeps_atomic_owned_gate() {
    tokio::time::timeout(Duration::from_secs(20), probe(false, false))
        .await
        .expect("clear must complete after releasing append barrier");
}

async fn probe(abort: bool, active: bool) {
    let dir = match std::env::var_os("RPI_NOTICE_TEST_ROOT") {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let path = dir
        .path()
        .join("queue.jsonl")
        .to_string_lossy()
        .replace('\\', "/");
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
    let storage = Arc::new(
        JsonlSessionStorage::create(
            fs.clone(),
            &path,
            JsonlV4Header::new(
                "queue-caller-drop".into(),
                1,
                dir.path().display().to_string(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    );
    let h = AgentHarness::create(AgentHarnessOptions {
        session: Session::new(storage.clone(), None),
        ..Default::default()
    })
    .await
    .unwrap();
    let steering = h
        .steer(UserMessage::new("steering", 0).into())
        .await
        .unwrap();
    let follow_up = h
        .follow_up(UserMessage::new("follow-up", 0).into())
        .await
        .unwrap();
    let next = h
        .next_run(UserMessage::new("next-run", 0).into())
        .await
        .unwrap();
    let active_run = if active {
        Some(h.acquire_run(OperationKind::Run).await.unwrap())
    } else {
        None
    };
    let gate = Arc::new(Gate::default());
    storage.install_append_handle(Box::new(CancellationAppend {
        fs: fs.clone(),
        path: path.clone(),
        gate: gate.clone(),
    }));
    let caller = {
        let h = h.clone();
        tokio::spawn(async move {
            if abort {
                h.abort_reclaiming(!active).await.map(|_| ())
            } else {
                h.clear_queue().await.map(|_| ())
            }
        })
    };
    gate.entered.notified().await;
    if active {
        assert!(h
            .inner
            .lock()
            .unwrap()
            .active_run
            .as_ref()
            .unwrap()
            .signal
            .is_cancelled());
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    // A caller-owned abort would release the gate here. Exactly one poll proves
    // that the shared gate is still owned by completion, without a timing race.
    let mut snapshot = Box::pin(h.queued_messages());
    poll_fn(|cx| match snapshot.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => panic!("queue escaped the cancellation gate: {result:?}"),
    })
    .await;
    gate.release.notify_one();
    h.session.drain().await;
    let visible = snapshot.await.unwrap(); // also waits for in-memory publication
    assert!(visible.steering.is_empty());
    assert!(visible.follow_up.is_empty());
    let cancellations = h
        .session
        .find_records(&RecordQuery {
            lane: Some("main".into()),
            record_type: Some("queue_cancelled"),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut ids: Vec<_> = cancellations
        .iter()
        .map(|record| {
            let LaneRecord::QueueCancelled(record) = record else {
                unreachable!()
            };
            record.entry_id.clone()
        })
        .collect();
    ids.sort();
    let mut expected = vec![steering.entry_id, follow_up.entry_id];
    expected.sort();
    assert_eq!(ids, expected);
    let bytes = fs.read_text_file(&path, None).await.unwrap();
    assert_eq!(
        bytes
            .lines()
            .filter(|line| line.contains("queue_cancelled"))
            .count(),
        2
    );
    let rebuilt = AgentHarness::rebuild_queues(&h.session, "main")
        .await
        .unwrap();
    assert_eq!(
        rebuilt.len(),
        1,
        "cancelled messages must not return on reconstruction"
    );
    assert_eq!(rebuilt[0].0, QueueKind::NextRun);
    assert_eq!(rebuilt[0].1.entry_id, next.entry_id);
    if let Some((run_id, _, _)) = active_run {
        h.release_run(&run_id);
    }
}
