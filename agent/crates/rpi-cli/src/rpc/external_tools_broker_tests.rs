#[tokio::test]
async fn malformed_outcomes_do_not_retire_waiter_and_duplicates_are_false() {
    let (writer, task, _) =
        super::super::writer::start_rpc_writer(std::io::sink(), Default::default());
    let backend = ExternalTools::detached(writer);
    let request = backend.broker.register().unwrap();
    let id = request.id.clone();
    for mut malformed in [
        json!({}),
        json!({"cancelled":false}),
        json!({"cancelled":null}),
        json!({"result":null}),
        json!({"error":null}),
        json!({"error":{}}),
        json!({"result":{"content":[]},"error":"bad"}),
        json!({"result":{"content":[]},"cancelled":true}),
        json!({"error":"bad","cancelled":true}),
        json!({"result":{"content":[],"isError":"true"}}),
        json!({"result":{"content":[{"type":"image","data":"!","mimeType":"image/png"}]}}),
        json!({"result":{"content":[{"type":"text","text":42}]}}),
    ] {
        malformed["requestId"] = json!(id);
        assert!(backend.respond(&malformed).is_err(), "{malformed}");
    }
    assert!(backend
        .respond(&json!({"requestId":id,"result":{"content":[]}}))
        .unwrap());
    assert!(!backend
        .respond(&json!({"requestId":id,"cancelled":true}))
        .unwrap());
    assert_eq!(
        request
            .wait(Duration::from_secs(1), &CancellationToken::new())
            .await
            .unwrap(),
        json!({"content":[]})
    );
    assert!(!backend
        .respond(&json!({"requestId":id,"error":"late"}))
        .unwrap());
    backend.shutdown();
    task.await.unwrap().unwrap();
}

#[test]
fn text_signatures_and_annotations_are_metadata_only() {
    let raw = json!({"content":[{"type":"text","text":"answer","textSignature":"untrusted","annotations":{"secret":true}}]});
    let result = decode_result(raw.clone()).unwrap();
    assert_eq!(result.content, vec![TextContentOrImage::text("answer")]);
    assert_eq!(result.details["externalToolResult"], raw);
}
