//! Versioned layout experiment, reachable only by this integration test.
#[path = "support/managed_recovery_prototype.rs"]
mod prototype;
use prototype::{Action, Event, Fault, Identity, Recovery, Result, Store, MAX_TAIL};
use rusqlite::{params, Connection};
use std::collections::HashSet;

fn identity() -> Identity {
    Identity {
        session: "synthetic-session".into(),
        generation: "generation-001".into(),
    }
}
fn event(id: impl Into<String>, lane: &str, action: Action) -> Event {
    Event {
        id: id.into(),
        lane: lane.into(),
        action,
    }
}
fn message(id: impl Into<String>, parent: Option<String>) -> Event {
    event(id, "main", Action::Message { parent })
}
fn error<T>(result: Result<T>, expected: &str) {
    let actual = result.err().expect("must reject").to_string();
    assert!(
        actual.contains(expected),
        "expected {expected:?}, got {actual:?}"
    );
}
fn db(path: &std::path::Path, sql: &str) {
    Connection::open(path).unwrap().execute_batch(sql).unwrap();
}

#[test]
fn checkpoint_tail_matches_full_metadata_replay_across_lanes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("managed.rpidb");
    let mut store = Store::create(&path, identity())?;
    let mut expected = Recovery::default();
    let mut committed = HashSet::new();
    let mut events = vec![
        event("lane-side", "side", Action::CreateLane { at: None }),
        event("run-main", "main", Action::Start),
        event("run-side", "side", Action::Start),
        event(
            "q-main",
            "main",
            Action::Enqueue {
                target: "shared".into(),
                queue: "steering".into(),
            },
        ),
        event(
            "q-side",
            "side",
            Action::Enqueue {
                target: "shared".into(),
                queue: "follow_up".into(),
            },
        ),
        event(
            "cancel-main",
            "main",
            Action::Cancel {
                target: "shared".into(),
            },
        ),
        event("shared", "side", Action::Message { parent: None }),
        event(
            "finish-main",
            "main",
            Action::Finish {
                run: "run-main".into(),
            },
        ),
        event(
            "move-main",
            "main",
            Action::MoveLane {
                to: Some("shared".into()),
            },
        ),
    ];
    let mut parent = Some("shared".into());
    for i in 0..35 {
        let id = format!("message-{i:03}");
        events.push(message(id.clone(), parent));
        parent = Some(id);
    }
    events.push(event(
        "pending",
        "main",
        Action::Enqueue {
            target: "future".into(),
            queue: "follow_up".into(),
        },
    ));
    events.push(event(
        "already-committed",
        "side",
        Action::Enqueue {
            target: "message-000".into(),
            queue: "steering".into(),
        },
    ));
    for (index, event) in events.into_iter().enumerate() {
        let body = if matches!(
            event.action,
            Action::Message { .. } | Action::Enqueue { .. }
        ) {
            b"opaque payload".as_slice()
        } else {
            &[]
        };
        expected.apply(&event, index as u64 + 1, |id| Ok(committed.contains(id)))?;
        store.append(event.clone(), body, Some(index as u64), Fault::None)?;
        if matches!(event.action, Action::Message { .. }) {
            committed.insert(event.id);
        }
        let (restored, reads) = store.restore()?;
        assert_eq!(restored, expected);
        assert!(reads.tail_rows < MAX_TAIL as usize);
        assert_eq!(reads.decoded_body_bytes, 0);
    }
    assert_eq!(expected.pending["main"].len(), 1);
    assert!(!expected.pending.contains_key("side"));
    assert_eq!(expected.operations["side"], ["run-side"]);
    drop(store);
    assert_eq!(Store::open(&path, identity())?.restore()?.0, expected);
    Ok(())
}

#[test]
fn restore_decode_volume_is_bounded_not_proportional_to_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut measurements = Vec::new();
    for count in [128, 2048] {
        let path = directory.path().join(format!("fixture-{count}.rpidb"));
        let mut store = Store::create(&path, identity())?;
        let body = vec![b'x'; 16 * 1024];
        let mut parent = None;
        for index in 0..count {
            let id = format!("m-{index:06}");
            store.append(message(id.clone(), parent), &body, None, Fault::None)?;
            parent = Some(id);
        }
        store.append(
            event("active-run", "main", Action::Start),
            &[],
            None,
            Fault::None,
        )?;
        store.append(
            event(
                "active-enqueue",
                "main",
                Action::Enqueue {
                    target: "pending-target".into(),
                    queue: "steering".into(),
                },
            ),
            &body,
            None,
            Fault::None,
        )?;
        drop(store);
        let mut reopened = Store::open(&path, identity())?;
        reopened.forbid_history_bodies(true);
        let (state, reads) = reopened.restore()?;
        assert_eq!(state.pending["main"].len(), 1);
        assert_eq!(state.operations["main"], ["active-run"]);
        assert_eq!(reads.tail_rows, 2);
        assert_eq!(reads.decoded_body_bytes, 0);
        assert_eq!(reopened.page("main", 0, 1)?.len(), 1);
        assert!(
            reopened.body("active-enqueue").is_err(),
            "SQL body-read guard must actually refuse payloads"
        );
        reopened.forbid_history_bodies(false);
        assert_eq!(reopened.body("active-enqueue")?.unwrap(), body);
        assert_eq!(reopened.body("m-000000")?.unwrap(), body);
        eprintln!("managed_restore_probe history_rows={} checkpoint_bytes={} tail_rows={} tail_bytes={} decoded_body_bytes={}", reopened.count()?, reads.checkpoint_bytes, reads.tail_rows, reads.tail_bytes, reads.decoded_body_bytes);
        measurements.push(reads);
    }
    assert_eq!(measurements[0].tail_bytes, measurements[1].tail_bytes);
    assert!(
        measurements[0]
            .checkpoint_bytes
            .abs_diff(measurements[1].checkpoint_bytes)
            < 16
    );
    Ok(())
}

#[test]
fn transaction_faults_roll_back_history_revision_and_checkpoint() -> Result<()> {
    for fault in [
        Fault::AfterHistory,
        Fault::DuringCheckpoint,
        Fault::BeforeCommit,
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("fault.rpidb");
        let mut store = Store::create(&path, identity())?;
        let mut parent = None;
        for index in 0..MAX_TAIL - 1 {
            let id = format!("before-{index}");
            store.append(message(id.clone(), parent), b"before", None, Fault::None)?;
            parent = Some(id);
        }
        let before = store.restore()?;
        error(
            store.append(
                message("failed", parent.clone()),
                b"not committed",
                None,
                fault,
            ),
            "injected failure",
        );
        assert_eq!(store.count()?, MAX_TAIL - 1);
        assert_eq!(store.mutation_sequence("failed")?, None);
        assert_eq!(store.restore()?, before);
        drop(store);
        let mut store = Store::open(&path, identity())?;
        assert_eq!(store.restore()?, before);
        store.append(
            message("success", parent),
            b"committed",
            Some(MAX_TAIL - 1),
            Fault::None,
        )?;
        assert_eq!(store.restore()?.1.tail_rows, 0);
    }
    Ok(())
}

#[test]
fn lost_ack_is_reconciled_by_id_without_reappending() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ack.rpidb");
    let mut store = Store::create(&path, identity())?;
    let first = message("maybe-committed", None);
    error(
        store.append(first.clone(), b"body", Some(0), Fault::LostAck),
        "outcome uncertain",
    );
    drop(store);
    let mut reopened = Store::open(&path, identity())?;
    assert_eq!(reopened.mutation_sequence(&first.id)?, Some(1));
    assert_eq!(reopened.restore()?.0.heads["main"], Some(first.id.clone()));
    assert!(reopened.append(first, b"body", None, Fault::None).is_err());
    assert_eq!(reopened.count()?, 1);
    Ok(())
}

#[test]
fn separate_handles_refresh_state_and_reject_stale_revision() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("handles.rpidb");
    let mut left = Store::create(&path, identity())?;
    let mut right = Store::open(&path, identity())?;
    left.append(message("first", None), b"one", Some(0), Fault::None)?;
    error(
        right.append(message("stale", None), b"two", Some(0), Fault::None),
        "stale expected revision",
    );
    right.append(
        message("second", Some("first".into())),
        b"two",
        Some(1),
        Fault::None,
    )?;
    left.append(
        message("third", Some("second".into())),
        b"three",
        None,
        Fault::None,
    )?;
    assert_eq!(left.restore()?, right.restore()?);
    assert_eq!(left.count()?, 3);
    Ok(())
}

#[test]
fn history_cursor_is_lane_scoped_and_body_corruption_is_lazy() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pages.rpidb");
    let mut store = Store::create(&path, identity())?;
    store.append(
        event("side", "side", Action::CreateLane { at: None }),
        &[],
        None,
        Fault::None,
    )?;
    let mut parent = None;
    for index in 0..39 {
        let id = format!("entry-{index:03}");
        store.append(message(id.clone(), parent), b"body", None, Fault::None)?;
        parent = Some(id);
    }
    store.append(
        event("side-entry", "side", Action::Message { parent: None }),
        b"other lane",
        None,
        Fault::None,
    )?;
    let mut cursor = 0;
    let mut ids = Vec::new();
    loop {
        let page = store.page("main", cursor, 7)?;
        if page.is_empty() {
            break;
        }
        assert!(page.iter().all(|(seq, _)| *seq > cursor));
        cursor = page.last().unwrap().0;
        ids.extend(page.into_iter().map(|(_, id)| id));
    }
    assert_eq!(
        ids,
        (0..39).map(|i| format!("entry-{i:03}")).collect::<Vec<_>>()
    );
    assert_eq!(store.page("side", 0, 7)?.len(), 1);
    assert!(store.page("main", 0, 0).is_err());
    assert!(store.page("main", 0, 129).is_err());
    db(&path, "UPDATE history SET body=X'ff' WHERE id='entry-000'");
    store.restore()?;
    error(store.body("entry-000"), "body digest mismatch");
    assert_eq!(store.body("entry-001")?, Some(b"body".to_vec()));
    assert_eq!(store.body("absent")?, None);
    Ok(())
}

#[test]
fn unknown_formats_and_foreign_identities_are_rejected_without_overwrite() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("identity.rpidb");
    drop(Store::create(&path, identity())?);
    let original = std::fs::read(&path)?;
    assert!(Store::create(&path, identity()).is_err());
    assert_eq!(std::fs::read(&path)?, original);
    let mut other = identity();
    other.session = "wrong-session".into();
    error(Store::open(&path, other), "store identity mismatch");
    let mut other = identity();
    other.generation = "wrong-generation".into();
    error(Store::open(&path, other), "store identity mismatch");
    assert_eq!(std::fs::read(&path)?, original);
    db(&path, "PRAGMA user_version=999");
    error(
        Store::open(&path, identity()),
        "unsupported managed store version",
    );
    db(
        &path,
        "PRAGMA user_version=1; UPDATE store_meta SET format='foreign-format'",
    );
    error(Store::open(&path, identity()), "store identity mismatch");
    let legacy = directory.path().join("legacy.jsonl");
    std::fs::write(&legacy, b"{\"kind\":\"header\",\"version\":4}\n")?;
    assert!(Store::open(&legacy, identity()).is_err());
    assert_eq!(
        std::fs::read(&legacy)?,
        b"{\"kind\":\"header\",\"version\":4}\n"
    );
    let missing = directory.path().join("missing.rpidb");
    assert!(Store::open(&missing, identity()).is_err());
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn checkpoint_and_tail_corruption_fail_closed() -> Result<()> {
    for (sql, expected) in [
        (
            "UPDATE checkpoint SET digest=zeroblob(32)",
            "checkpoint digest mismatch",
        ),
        (
            "UPDATE checkpoint SET state_json=printf('%140000s','x')",
            "checkpoint exceeds active state budget",
        ),
        ("DELETE FROM checkpoint", "Query returned no rows"),
        (
            "DELETE FROM history WHERE seq=2",
            "nonconsecutive recovery sequence",
        ),
        (
            "UPDATE history SET event_digest=zeroblob(32) WHERE seq=2",
            "tail metadata digest mismatch",
        ),
        (
            "UPDATE history SET event_json=printf('%20000s','x') WHERE seq=2",
            "recovery metadata exceeds budget",
        ),
        (
            "UPDATE store_meta SET revision=17",
            "recovery tail exceeds budget",
        ),
        (
            "UPDATE store_meta SET revision=2",
            "history revision mismatch",
        ),
        (
            "UPDATE store_meta SET session=printf('%20000s','x')",
            "store identity mismatch",
        ),
        (
            "UPDATE checkpoint SET digest=zeroblob(200000)",
            "checkpoint digest mismatch",
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("corrupt.rpidb");
        let mut store = Store::create(&path, identity())?;
        for index in 0..3 {
            store.append(
                event(format!("run-{index}"), "main", Action::Start),
                &[],
                None,
                Fault::None,
            )?;
        }
        drop(store);
        db(&path, sql);
        let before = std::fs::read(&path)?;
        error(Store::open(&path, identity()), expected);
        assert_eq!(
            std::fs::read(&path)?,
            before,
            "failed restore must not repair {sql}"
        );
    }
    Ok(())
}

#[test]
fn checkpoint_identity_binding_is_validated_even_with_matching_digest() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let left_path = directory.path().join("left.rpidb");
    let right_path = directory.path().join("right.rpidb");
    drop(Store::create(&left_path, identity())?);
    let mut other = identity();
    other.generation = "generation-002".into();
    drop(Store::create(&right_path, other)?);
    let foreign = Connection::open(&right_path)?.query_row(
        "SELECT state_json,digest FROM checkpoint",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    Connection::open(&left_path)?.execute(
        "UPDATE checkpoint SET state_json=?1,digest=?2",
        params![foreign.0, foreign.1],
    )?;
    error(
        Store::open(&left_path, identity()),
        "checkpoint identity mismatch",
    );
    Ok(())
}

#[test]
fn invalid_events_and_unique_ids_never_consume_a_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("invalid.rpidb");
    let mut store = Store::create(&path, identity())?;
    for bad in [
        event("", "main", Action::Start),
        event("missing-lane", "absent", Action::Start),
        event(
            "bad-move",
            "main",
            Action::MoveLane {
                to: Some("missing-entry".into()),
            },
        ),
        event(
            "bad-parent",
            "main",
            Action::Message {
                parent: Some("missing-entry".into()),
            },
        ),
        event(
            "bad-run",
            "main",
            Action::Finish {
                run: "missing-run".into(),
            },
        ),
        event(
            "bad-queue",
            "main",
            Action::Enqueue {
                target: "future".into(),
                queue: "bad-kind".into(),
            },
        ),
    ] {
        assert!(store.append(bad, &[], None, Fault::None).is_err());
        assert_eq!(store.restore()?.0.sequence, 0);
        assert_eq!(store.count()?, 0);
    }
    store.append(event("run", "main", Action::Start), &[], None, Fault::None)?;
    assert!(store
        .append(event("run", "main", Action::Start), &[], None, Fault::None)
        .is_err());
    assert_eq!(store.count()?, 1);
    assert_eq!(store.restore()?.0.sequence, 1);
    Ok(())
}

#[test]
fn concurrent_writers_use_one_transactional_revision() -> Result<()> {
    use std::sync::{Arc, Barrier};
    for expected in [Some(0), None] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("concurrent.rpidb");
        let left = Store::create(&path, identity())?;
        let right = Store::open(&path, identity())?;
        let barrier = Arc::new(Barrier::new(2));
        let handles = [left, right]
            .into_iter()
            .enumerate()
            .map(|(index, mut store)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .append(
                            event(format!("concurrent-{index}"), "main", Action::Start),
                            &[],
                            expected,
                            Fault::None,
                        )
                        .map_err(|e| e.to_string())
                })
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().expect("writer must not panic"))
            .collect::<Vec<_>>();
        let expected_count = if expected.is_some() { 1 } else { 2 };
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            expected_count
        );
        for error in results.iter().filter_map(|result| result.as_ref().err()) {
            assert!(error.contains("stale expected revision"), "{error}");
        }
        let mut reopened = Store::open(&path, identity())?;
        assert_eq!(reopened.count()?, expected_count as u64);
        assert_eq!(
            reopened.restore()?.0.operations["main"].len(),
            expected_count
        );
    }
    Ok(())
}

#[test]
fn oversized_body_and_duplicate_queue_target_roll_back() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("budgets.rpidb");
    let mut store = Store::create(&path, identity())?;
    error(
        store.append(
            message("huge", None),
            &vec![0; 256 * 1024 + 1],
            None,
            Fault::None,
        ),
        "body exceeds prototype limit",
    );
    assert_eq!(store.count()?, 0);
    let first = event(
        "enqueue-one",
        "main",
        Action::Enqueue {
            target: "future".into(),
            queue: "steering".into(),
        },
    );
    store.append(first.clone(), b"one", None, Fault::None)?;
    let mut duplicate = first;
    duplicate.id = "enqueue-two".into();
    assert!(store.append(duplicate, b"two", None, Fault::None).is_err());
    assert_eq!(store.count()?, 1);
    assert_eq!(
        store.restore()?.0.pending["main"]["future"].enqueue,
        "enqueue-one"
    );
    Ok(())
}

#[test]
fn actual_sql_checkpoint_error_rolls_back_the_entire_append() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("checkpoint-error.rpidb");
    let mut store = Store::create(&path, identity())?;
    for index in 0..MAX_TAIL - 1 {
        store.append(
            event(format!("run-{index}"), "main", Action::Start),
            &[],
            None,
            Fault::None,
        )?;
    }
    let before = store.restore()?;
    db(&path, "CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON checkpoint BEGIN SELECT RAISE(ABORT,'checkpoint refused'); END");
    let last = event("last-run", "main", Action::Start);
    error(
        store.append(last.clone(), &[], None, Fault::None),
        "checkpoint refused",
    );
    assert_eq!(store.restore()?, before);
    assert_eq!(store.mutation_sequence("last-run")?, None);
    db(&path, "DROP TRIGGER reject_checkpoint");
    assert_eq!(store.append(last, &[], None, Fault::None)?, MAX_TAIL);
    assert_eq!(store.restore()?.1.tail_rows, 0);
    Ok(())
}
