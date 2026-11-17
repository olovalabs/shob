struct Blocked {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.entered.send(());
        let _ = self.release.recv();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn stalled_writer() -> (
    RpcWriter,
    tokio::task::JoinHandle<std::io::Result<()>>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, received) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (writer, task, _) = start_rpc_writer(
        Blocked {
            entered,
            release: gate,
        },
        Default::default(),
    );
    write_line(&writer, &serde_json::json!(0)).await;
    tokio::task::spawn_blocking(move || received.recv().unwrap())
        .await
        .unwrap();
    for n in 0..64 {
        write_line(&writer, &serde_json::json!(n)).await;
    }
    (writer, task, release)
}

#[tokio::test]
async fn external_deadline_wakes_ordinary_queued_producer() {
    let (writer, task, release) = stalled_writer().await;
    let producer = {
        let writer = writer.clone();
        tokio::spawn(async move { write_line(&writer, &serde_json::json!("ordinary")).await })
    };
    let result = writer.write_bounded(&serde_json::json!("external")).await;
    // Release before assertions: a test failure must not strand OS IO.
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), producer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(writer.failure().is_cancelled());
    drop(writer);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn full_closure_queue_is_authoritative_failure() {
    let (writer, task, release) = stalled_writer().await;
    writer.try_write_line(&serde_json::json!({"type":"external_tool_request_closed"}));
    release.send(()).unwrap();
    assert!(writer.failure().is_cancelled());
    drop(writer);
    assert!(task.await.unwrap().is_err());
}
