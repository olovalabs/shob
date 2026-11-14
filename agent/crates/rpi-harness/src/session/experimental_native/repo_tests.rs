use super::*;
use std::sync::mpsc;

struct AdmissionClock {
    started: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Clock for AdmissionClock {
    fn now_ms(&self) -> i64 {
        if let Some(sender) = self.started.lock().unwrap().take() {
            sender.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        42
    }
}
#[tokio::test]
async fn lifecycle_owner_retains_gate_after_create_caller_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let repo = ExperimentalNativeRepo::with_clock(
        dir.path().to_owned(),
        Arc::new(AdmissionClock {
            started: Mutex::new(Some(start_tx)),
            release: Mutex::new(release_rx),
        }),
    )
    .await
    .unwrap();
    let writer = repo.clone();
    let create = tokio::spawn(async move {
        writer
            .create(&SessionCreateOptions {
                id: Some("cancelled-create".into()),
                ..Default::default()
            })
            .await
    });
    tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
        .await
        .unwrap();
    create.abort();
    let _ = create.await;
    let listing = repo.list();
    tokio::pin!(listing);
    assert!(
        futures::poll!(&mut listing).is_pending(),
        "list must wait for the admitted create owner"
    );
    release_tx.send(()).unwrap();
    let catalog = listing.await.unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "cancelled-create");
    let storage = repo.open(&catalog[0]).await.unwrap();
    assert_eq!(storage.history_revision().await.unwrap(), 0);
    assert!(storage.read_progress("run").await.unwrap().is_empty());
}
#[tokio::test]
async fn failed_initialization_does_not_publish_a_partial_schema_or_overwrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rejected.rpinative");
    let identity = Identity {
        session: "rejected".into(),
        generation: "generation".into(),
    };
    let result = Store::create_initialized(&path, identity.clone(), |db, identity| {
        publish_aux(
            db,
            identity,
            "metadata",
            &SessionMetadata {
                id: "rejected".into(),
                created_at: 42,
                parent_session_id: None,
            },
        )?;
        Err("injected initialization failure".into())
    });
    assert!(result.is_err());
    let bytes = std::fs::read(&path).unwrap();
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
    drop(db);
    assert!(Store::open(&path, identity.clone()).is_err());
    assert!(Store::create(&path, identity).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
#[tokio::test]
async fn catalog_rejects_a_misnamed_or_foreign_experimental_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("fixture".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    drop(storage);
    std::fs::rename(
        dir.path().join("fixture.rpinative"),
        dir.path().join("mismatch.rpinative"),
    )
    .unwrap();
    assert!(repo.list().await.is_err());
    std::fs::rename(
        dir.path().join("mismatch.rpinative"),
        dir.path().join("fixture.rpinative"),
    )
    .unwrap();
    assert_eq!(repo.list().await.unwrap().len(), 1);
    let path = dir.path().join("foreign.rpinative");
    std::fs::write(&path, b"not sqlite").unwrap();
    assert!(repo.list().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
}

async fn registration_reclaims_expired_handles(fresh_open: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let live = repo
        .create(&SessionCreateOptions {
            id: Some("live".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut saved = Vec::new();
    for i in 0..8 {
        let storage = repo
            .create(&SessionCreateOptions {
                id: Some(format!("retired-{i}")),
                ..Default::default()
            })
            .await
            .unwrap();
        saved.push(storage.metadata());
        drop(storage);
    }
    let trigger = if fresh_open {
        repo.open(&saved[0]).await.unwrap()
    } else {
        repo.create(&SessionCreateOptions {
            id: Some("trigger".into()),
            ..Default::default()
        })
        .await
        .unwrap()
    };
    // Never assert while holding the global mutex: a negative test must not
    // poison the table and manufacture unrelated parallel test failures.
    let live_path = addressed(&repo.root, "live").unwrap();
    let (dead, shared) = {
        let registry = handles().lock().unwrap();
        let dead = registry
            .iter()
            .filter(|(path, weak)| path.starts_with(&repo.root) && weak.strong_count() == 0)
            .count();
        let shared = registry
            .get(&live_path)
            .and_then(Weak::upgrade)
            .is_some_and(|inner| Arc::ptr_eq(&inner, &live.inner));
        (dead, shared)
    };
    assert_eq!(
        dead, 0,
        "new registration retained expired entries for this root"
    );
    assert!(shared, "cleanup lost the still-active handle");
    let reopened = repo.open(&live.metadata()).await.unwrap();
    assert!(Arc::ptr_eq(&reopened.inner, &live.inner));
    assert_eq!(reopened.history_revision().await.unwrap(), 0);
    drop(reopened);
    drop(trigger);
    drop(live);
    drop(repo);
    dir.close().unwrap();
}
#[tokio::test]
async fn create_registration_reclaims_expired_handles() {
    registration_reclaims_expired_handles(false).await;
}
#[tokio::test]
async fn fresh_open_registration_reclaims_expired_handles() {
    registration_reclaims_expired_handles(true).await;
}

#[tokio::test]
async fn atomic_reservation_conflict_is_already_exists_without_overwrite() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let repo = ExperimentalNativeRepo::with_clock(
        dir.path().to_owned(),
        Arc::new(AdmissionClock {
            started: Mutex::new(Some(start_tx)),
            release: Mutex::new(release_rx),
        }),
    )
    .await
    .unwrap();
    let options = SessionCreateOptions {
        id: Some("reservation-race".into()),
        ..Default::default()
    };
    let writer = repo.clone();
    let request = options.clone();
    let task = tokio::spawn(async move { writer.create(&request).await });
    tokio::task::spawn_blocking(move || {
        start_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
    })
    .await
    .unwrap();
    // The clock is after exists() but before create_new(). This actor bypasses
    // the in-process repo gate, just as an independent filesystem writer can.
    let path = dir.path().join("reservation-race.rpinative");
    let sentinel = b"synthetic conflicting reservation\n";
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(sentinel).unwrap();
    drop(file);
    release_tx.send(()).unwrap();
    let failure = match task.await.unwrap() {
        Ok(_) => panic!("create replaced a conflicting reservation"),
        Err(e) => e,
    };
    assert_eq!(failure.code, crate::error::SessionErrorCode::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), sentinel);
    let published = handles()
        .lock()
        .unwrap()
        .contains_key(&addressed(&repo.root, "reservation-race").unwrap());
    assert!(!published, "failed reservation published a handle");
    for suffix in ["-journal", "-wal", "-shm"] {
        assert!(!dir
            .path()
            .join(format!("reservation-race.rpinative{suffix}"))
            .exists());
    }
    let retry = match repo.create(&options).await {
        Ok(_) => panic!("existing reservation overwritten"),
        Err(e) => e,
    };
    assert_eq!(retry.code, crate::error::SessionErrorCode::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), sentinel);
    drop(repo);
    dir.close().unwrap();
}

#[test]
fn creation_error_only_classifies_atomic_reservation_already_exists() {
    use super::super::engine::ReservationError;
    use crate::error::SessionErrorCode;
    use std::io::{Error, ErrorKind};
    for kind in [
        ErrorKind::AlreadyExists,
        ErrorKind::PermissionDenied,
        ErrorKind::NotFound,
        ErrorKind::Other,
    ] {
        let raw = Error::new(kind, "synthetic reservation error");
        let message = raw.to_string();
        let wrapped = ReservationError(raw);
        assert_eq!(wrapped.to_string(), message);
        let source = std::error::Error::source(&wrapped)
            .unwrap()
            .downcast_ref::<Error>()
            .unwrap();
        assert_eq!(source.kind(), kind);
        let classified = create_error(Box::new(wrapped));
        assert_eq!(
            classified.code,
            if kind == ErrorKind::AlreadyExists {
                SessionErrorCode::AlreadyExists
            } else {
                SessionErrorCode::Storage
            }
        );
    }
    let unrelated = create_error(Box::new(Error::new(
        ErrorKind::AlreadyExists,
        "unrelated IO error",
    )));
    assert_eq!(unrelated.code, SessionErrorCode::Storage);
    assert_eq!(unrelated.message, "unrelated IO error");
}
#[test]
fn initializer_already_exists_is_not_an_artifact_reservation_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("initializer.rpinative");
    let result = Store::create_initialized(
        &path,
        Identity {
            session: "initializer".into(),
            generation: "synthetic-generation".into(),
        },
        |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "synthetic initializer error",
            )
            .into())
        },
    )
    .map_err(create_error);
    let failure = match result {
        Ok(_) => panic!("initializer unexpectedly succeeded"),
        Err(e) => e,
    };
    assert_eq!(failure.code, crate::error::SessionErrorCode::Storage);
    assert_eq!(failure.message, "synthetic initializer error");
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "initializer failure published partial schema");
    drop(db);
    dir.close().unwrap();
}
