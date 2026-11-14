use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_parent_can_reclaim_children_but_never_start_more_work() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("research");
    let signal = CancellationToken::new();
    let parent = plugin.for_invocation(false, signal.clone());
    let child = plugin.for_invocation(true, CancellationToken::new());
    signal.cancel();
    for op in ["poll", "cancel", "dispose"] {
        for (bridge, key, expected) in [
            (&parent, parent.invocation_key().unwrap(), 0),
            (&parent, "guessed", 1),
            (&child, child.invocation_key().unwrap(), 1),
        ] {
            let reply = call(
                bridge.user_data() as usize,
                json!({"op":op,"childId":"retained","__rpiRuntimeKey":key}),
            );
            assert_eq!(reply.0, expected, "{op}: {reply:?}");
        }
    }
    assert_eq!(
        call(
            parent.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()})
        )
        .0,
        1
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 3);
}
