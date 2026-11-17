use super::*;
use crate::session::{experimental_native::ExperimentalNativeRepo, memory::SystemClock};

async fn fixture() -> (tempfile::TempDir, ExperimentalNativeStorage) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
    (dir, storage)
}
fn start(id: &str) -> SessionMutation {
    SessionMutation::Record{record:serde_json::from_value(serde_json::json!({"id":id,"lane":"main","seq":0,"timestamp":42,"type":"operation_started","sourceLeafId":null,"intent":{"kind":"run","originalPrompt":[],"initialMessages":[]}})).unwrap()}
}
#[tokio::test]
async fn open_operation_limit_precedes_body_materialization() {
    let (_dir, storage) = fixture().await;
    storage
        .run(|store, clock| {
            let writes = (1..=3)
                .map(|seq| {
                    let mut mutation = start(&format!("run-{seq}"));
                    if let SessionMutation::Record {
                        record: LaneRecord::OperationStarted(ref mut r),
                    } = mutation
                    {
                        r.base.seq = seq;
                    }
                    Write::Raw(mutation)
                })
                .collect();
            commit(store, writes, None, clock)?;
            // Deliberately corrupt the oldest body; recovery and a cap of two must not read it.
            store.connection.execute(
                "UPDATE facts SET body='damaged oldest body' WHERE seq=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let two = storage.find_open_operations("main", Some(2)).await.unwrap();
    assert_eq!(
        two.iter().map(|r| r.base.id.as_str()).collect::<Vec<_>>(),
        vec!["run-3", "run-2"]
    );
    assert!(storage.find_open_operations("main", None).await.is_err());
}
#[tokio::test]
async fn unrelated_damaged_record_body_is_not_read_by_entry_lookup() {
    let (_dir, storage) = fixture().await;
    storage
        .run(|store, clock| {
            let record = match start("record-only") {
                SessionMutation::Record { record } => record,
                _ => unreachable!(),
            };
            commit(store, vec![Write::Record(record)], None, clock)?;
            store.connection.execute(
                "UPDATE facts SET body='damaged record body' WHERE seq=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(storage.get_entry("record-only").await.unwrap().is_none());
    assert!(storage.find_records(&RecordQuery::default()).await.is_err());
}
#[tokio::test]
async fn a_terminal_index_without_valid_metadata_cannot_prune_progress() {
    let (_dir, storage) = fixture().await;
    let frame:AssistantFrameRecord=serde_json::from_value(serde_json::json!({"id":"frame","seq":0,"timestamp":0,"lane":"main","runId":"run","streamIndex":0,"op":"append","frame":{"delta":"keep"}})).unwrap();
    storage.append_progress(vec![frame]).await.unwrap();
    let finish:LaneRecord=serde_json::from_value(serde_json::json!({"id":"finish","seq":0,"timestamp":0,"lane":"main","type":"operation_finished","runId":"run","outcome":"completed"})).unwrap();
    storage.append_record(finish).await.unwrap();
    storage
        .run(|store, _| {
            store.connection.execute(
                "UPDATE facts SET metadata='damaged proof metadata' WHERE id='finish'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(storage.prune_completed_progress().await.is_err());
    assert_eq!(storage.read_progress("run").await.unwrap().len(), 1);
}
#[tokio::test]
async fn independent_connections_use_fresh_projection_and_sql_transaction_fences() {
    let (dir, storage) = fixture().await;
    let path = dir
        .path()
        .join(format!("{}.rpinative", storage.metadata.id));
    let identity = storage.run(|s, _| Ok(s.identity.clone())).await.unwrap();
    let independent = ExperimentalNativeStorage::from_store(
        Store::open(&path, identity).unwrap(),
        storage.metadata(),
        Arc::new(SystemClock),
    );
    let mut tasks = Vec::new();
    for n in 0..30 {
        let handle = if n % 2 == 0 {
            storage.clone()
        } else {
            independent.clone()
        };
        tasks.push(tokio::spawn(async move {
            handle
                .write(vec![Write::Name(Some(format!("writer-{n}")))], None)
                .await
                .unwrap()[0]
                .seq()
        }));
    }
    let mut sequences = Vec::new();
    for task in tasks {
        sequences.push(task.await.unwrap());
    }
    sequences.sort();
    assert_eq!(sequences, (1..=30).collect::<Vec<_>>());
    assert_eq!(storage.history_revision().await.unwrap(), 30);
    assert_eq!(independent.history_revision().await.unwrap(), 30);
    assert!(independent
        .write(vec![Write::Name(None)], Some(0))
        .await
        .is_err());
}
