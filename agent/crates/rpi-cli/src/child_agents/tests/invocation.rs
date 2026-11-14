use super::*;
use rpi_agent::tool_execution_scope::with_invocation_signal;

#[tokio::test]
async fn a_start_queued_by_an_expired_invocation_cannot_attach_to_the_idle_root() {
    let parent = parent(FauxScript::new().with_text("must not run")).await;
    let manager = ChildAgents::default();
    let signal = CancellationToken::new();
    signal.cancel();
    let result = with_invocation_signal(
        signal,
        manager.handle(
            &parent,
            7,
            json!({"op":"start","task":"expired queued start"}),
        ),
    )
    .await;
    // Always stop any incorrectly admitted work before reporting the failure.
    manager.cancel_all();
    assert!(
        result.is_err(),
        "cancelled invocation admitted a new child: {result:?}"
    );
    assert!(manager.jobs.lock().unwrap().is_empty());
    assert_eq!(manager.active.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_root_child_observes_the_originating_tool_cancellation() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = ChildAgents::default();
    let signal = CancellationToken::new();
    let child = with_invocation_signal(
        signal.clone(),
        manager.handle(
            &parent,
            7,
            json!({"op":"start","task":"cancel through invocation"}),
        ),
    )
    .await
    .unwrap();
    signal.cancel();
    let id = child["childId"].as_str().unwrap();
    let terminal =
        tokio::time::timeout(Duration::from_millis(250), wait(&manager, &parent, id)).await;
    manager.cancel_all();
    let value = terminal
        .expect("child must observe invocation cancellation, not only root admission state");
    assert!(value["error"].as_str().unwrap().contains("invocation"));
}
