//! Unsupported headers must be rejected before SQLite can replay their journals.
use super::*;

#[test]
fn process_foreign_and_old_hot_journals_are_rejected_without_playback() {
    runtime().block_on(async {
        for (app, version) in [
            (0_u32, 0_u32),
            (0x5250494e, 1),
            (0x5250494f, 2),
            (0x52504951, 2),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = fixture(dir.path()).await;
            let s = open(&path).await;
            let metadata = s.metadata();
            let identity = s.run(|store, _| Ok(store.identity.clone())).await.unwrap();
            drop(s);
            let db =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .unwrap();
            db.execute_batch(&format!(
                "PRAGMA application_id={app}; PRAGMA user_version={version};"
            ))
            .unwrap();
            drop(db);
            let baseline = raw::main_bytes(&path);
            let mut child = ChildProcess::spawn(
                dir.path(),
                "unsupported-spill",
                json!({
                    "path":path,"kind":"unsupported-spill","stage":"unused"
                }),
            );
            child.event("opened");
            child.send("execute");
            assert_eq!(child.event("stopped")["stage"], "unsupported:before");
            let evidence = raw::evidence(&path, &baseline);
            eprintln!("unsupported header app={app:#x} version={version}: {evidence}");
            child.kill_and_reap();
            let before = raw::main_bytes(&path);
            let journal = raw::journal_path(&path);
            let journal_before = raw::main_bytes(&journal);
            let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
                .await
                .unwrap();
            assert!(Store::open(&path, identity).is_err());
            assert!(
                raw::main_bytes(&path) == before,
                "Store modified unsupported main file"
            );
            assert!(
                raw::main_bytes(&journal) == journal_before,
                "Store replayed unsupported journal"
            );
            assert!(repo.open(&metadata).await.is_err());
            assert!(
                raw::main_bytes(&path) == before,
                "repo.open modified unsupported main file"
            );
            assert!(
                raw::main_bytes(&journal) == journal_before,
                "repo.open replayed unsupported journal"
            );
            assert!(repo.list().await.is_err());
            assert!(
                raw::main_bytes(&path) == before,
                "repo.list modified unsupported main file"
            );
            assert!(
                raw::main_bytes(&journal) == journal_before,
                "repo.list replayed unsupported journal"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                3,
                "only main/journal/helper-dir expected"
            );
            drop(repo);
            drop(child);
            dir.close().unwrap();
        }
    });
}

#[test]
fn process_recognized_header_can_recover_but_never_accept_a_bad_root_format() {
    runtime().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path()).await;
        let s = open(&path).await;
        let metadata = s.metadata();
        drop(s);
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
        db.execute(
            "UPDATE root SET format='rpi-native-fact-experimental-v2'",
            [],
        )
        .unwrap();
        drop(db);
        let baseline = raw::main_bytes(&path);
        assert!(
            engine::validate_header(&path).is_ok(),
            "header remains recognized v3"
        );
        let mut child = ChildProcess::spawn(
            dir.path(),
            "bad-root-spill",
            json!({
                "path":path,"kind":"unsupported-spill","stage":"unused"
            }),
        );
        child.event("opened");
        child.send("execute");
        assert_eq!(child.event("stopped")["stage"], "unsupported:before");
        raw::evidence(&path, &baseline);
        child.kill_and_reap();
        let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
            .await
            .unwrap();
        assert!(
            repo.open(&metadata).await.is_err(),
            "bad logical root must not be accepted"
        );
        // Recognition permits SQLite recovery, not trust in the recovered root.
        // Unlike unsupported-header cases, an error can retire the crash journal.
        assert!(!raw::journal_path(&path).exists());
        assert!(
            raw::main_bytes(&path) == baseline,
            "rollback must retain the bad baseline, not repair its format"
        );
        assert!(repo.list().await.is_err());
        drop(repo);
        drop(child);
        dir.close().unwrap();
    });
}
#[test]
fn process_header_gate_rejects_partial_bad_and_missing_artifacts_without_creation() {
    runtime().block_on(async {
        let seed = tempfile::tempdir().unwrap();
        let seed_path = fixture(seed.path()).await;
        let s = open(&seed_path).await;
        let metadata = s.metadata();
        let identity = s.run(|store, _| Ok(store.identity.clone())).await.unwrap();
        drop(s);
        let header = raw::main_bytes(&seed_path)[..100].to_vec();
        assert!(engine::validate_header(&seed_path).is_ok());
        let mut cases = vec![
            None,
            Some(Vec::new()),
            Some(header[..1].to_vec()),
            Some(header[..15].to_vec()),
            Some(header[..99].to_vec()),
        ];
        for offset in [0, 16, 17, 18, 19, 60, 68] {
            let mut bad = header.clone();
            if offset == 16 || offset == 17 {
                bad[16] = 0;
                bad[17] = 3;
            } else {
                bad[offset] = 255;
            }
            cases.push(Some(bad));
        }
        for (index, bytes) in cases.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture.rpinative");
            if let Some(bytes) = &bytes {
                std::fs::write(&path, bytes).unwrap();
            }
            let journal = raw::journal_path(&path);
            std::fs::write(&journal, b"untouched fixture sidecar").unwrap();
            let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
                .await
                .unwrap();
            // Prove the OS gate itself rejects each field; a later SQLite error
            // on a truncated/invalid pager header is not sufficient evidence.
            assert!(
                engine::validate_header(&path).is_err(),
                "gate accepted header case {index}"
            );
            assert!(
                Store::open(&path, identity.clone()).is_err(),
                "accepted header case {index}"
            );
            assert!(
                repo.open(&metadata).await.is_err(),
                "accepted header case {index}"
            );
            // A missing file does not appear in list(), but an existing bad file must fail.
            if bytes.is_some() {
                assert!(repo.list().await.is_err());
            } else {
                assert!(repo.list().await.unwrap().is_empty());
            }
            assert_eq!(std::fs::read(&path).ok(), bytes, "header case {index}");
            assert_eq!(
                std::fs::read(&journal).unwrap(),
                b"untouched fixture sidecar"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                if path.exists() { 2 } else { 1 }
            );
            drop(repo);
            dir.close().unwrap();
        }
        seed.close().unwrap();
    });
}
