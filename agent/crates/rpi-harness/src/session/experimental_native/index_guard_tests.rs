use super::*;

#[tokio::test]
async fn excess_tool_mirrors_fail_after_canonical_count_plus_one_not_a_fixed_large_batch() {
    let mut counts = Vec::new();
    for count in [1, 1024] {
        let (_dir, s) = fixture().await;
        users(&s, 32).await;
        let steps = s
            .run(move |s, _| {
                let tx = s.connection.transaction()?;
                for n in 0..count {
                    tx.execute(
                        "INSERT INTO tool_keys VALUES(?1,32)",
                        [format!("{n:04}-{}", "x".repeat(256))],
                    )?;
                }
                tx.commit()?;
                let steps = Arc::new(AtomicUsize::new(0));
                let counter = steps.clone();
                s.connection.progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                );
                let result = engine::read_metadata(&s.connection, &s.identity, 32);
                s.connection.progress_handler(0, None::<fn() -> bool>);
                assert!(result.is_err());
                Ok(steps.load(Ordering::Relaxed))
            })
            .await
            .unwrap();
        counts.push(steps);
    }
    assert!(counts[0] > 0);
    assert!(
        counts[1] <= counts[0] + 20,
        "VM-step growth indicates an unbounded mirror scan: {counts:?}"
    );
}
#[tokio::test]
async fn canonical_tool_duplicates_are_deduplicated_but_missing_and_oversized_mirrors_fail() {
    let (_dir, s) = fixture().await;
    let mut assistant =
        rpi_ai::AssistantMessage::empty(rpi_ai::Api::AnthropicMessages, "fixture", "fixture", 42);
    assistant.content =
        vec![rpi_ai::Content::tool_call("tool", "fixture", serde_json::json!({})); 2];
    let item = ProvisionedEntry {
        id: "assistant".into(),
        kind: ProvisionedKind::Message {
            message: assistant.into(),
            terminate: None,
        },
    };
    s.append_entry(item, "main").await.unwrap();
    assert!(s.get_entry("assistant").await.unwrap().is_some());
    s.run(|s, _| {
        s.connection
            .execute("DELETE FROM tool_keys WHERE seq=1", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(s.get_entry("assistant").await.is_err());
    s.run(|s, _| {
        s.connection
            .execute("INSERT INTO tool_keys VALUES(?1,1)", ["x".repeat(65536)])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(s.get_entry("assistant").await.is_err());
}
#[tokio::test]
async fn old_versions_and_root_formats_are_rejected_without_rewriting_artifacts() {
    for sql in [
        "PRAGMA user_version=1",
        "PRAGMA user_version=2",
        "UPDATE root SET format='rpi-native-fact-prototype-v1'",
        "UPDATE root SET format='rpi-native-fact-experimental-v2'",
        "PRAGMA application_id=1380993358",
        "PRAGMA application_id=1380993359",
    ] {
        let (dir, s) = fixture().await;
        let metadata = s.metadata();
        let identity = s
            .run(move |s, _| {
                s.connection.execute_batch(sql)?;
                Ok(s.identity.clone())
            })
            .await
            .unwrap();
        drop(s);
        let path = dir.path().join(format!("{}.rpinative", metadata.id));
        let before = std::fs::read(&path).unwrap();
        assert!(Store::open(&path, identity).is_err(), "accepted {sql}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "rewrote {sql}");
    }
}
