use super::*;
use serde_json::{json, Value};
use std::sync::atomic::AtomicUsize;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    calls: AtomicUsize,
    revoked: AtomicUsize,
    aborts: AtomicUsize,
    command_lists: AtomicUsize,
    stop_rounds: AtomicUsize,
    side_sinks: std::sync::Mutex<Vec<SideQuestionSink>>,
}
#[async_trait::async_trait]
impl RuntimeActionHost for Host {
    fn stop_round(&self, args: Value) -> Result<Value, String> {
        assert!(args.get("__rpiRuntimeKey").is_none());
        self.stop_rounds.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"requested":true}))
    }

    async fn list_commands(&self, args: Value) -> Result<Value, String> {
        assert!(args.get("__rpiRuntimeKey").is_none());
        self.command_lists.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"commands":[{"name":"integrated","description":"both actions coexist","source":"extension"}]}),
        )
    }

    async fn side_question(
        &self,
        owner: u64,
        args: Value,
        sink: SideQuestionSink,
    ) -> Result<Value, String> {
        assert!(args.get("__rpiRuntimeKey").is_none());
        sink(json!({"op":"register","id":"btw-test","type":"text","slot":"overlay"}))?;
        self.side_sinks.lock().unwrap().push(sink);
        Ok(json!({"owner":owner}))
    }
    async fn abort(&self, args: Value) -> Result<Value, String> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        assert!(
            args.get("__rpiRuntimeKey").is_none(),
            "capabilities must not reach action payloads"
        );
        // No run is active in this mock: mirror the idle-lane no-op contract so
        // the test also pins "idle abort is not an error".
        Ok(json!({"aborted": false}))
    }
    async fn send_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn send_user_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn append_entry(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_session_name(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_model(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn compact(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_system_prompt(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn new_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn fork(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn navigate_tree(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn switch_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn reload(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn child_agent(&self, owner: u64, args: Value) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            rpi_agent::tool_execution_scope::invocation_signal().is_some(),
            "dispatch must retain the originating invocation's cancellation"
        );
        assert!(
            args.get("__rpiRuntimeKey").is_none(),
            "capabilities must not reach action payloads"
        );
        Ok(json!({"owner":owner}))
    }
    fn cancel_child_agents(&self, _: u64) {
        self.revoked.fetch_add(1, Ordering::SeqCst);
    }
}
fn call(id: usize, args: Value) -> (i32, Value) {
    let mut out = StbString::empty();
    let text = args.to_string();
    let rc = trampoline_runtime_action(
        RuntimeActionId::ChildAgent.into(),
        StbStringRef::from_str(&text),
        &mut out,
        id as *mut c_void,
    );
    // SAFETY: the trampoline returned a live host allocation to its caller.
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
    };
    (rc, serde_json::from_str(&owned.to_string_lossy()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_actions_require_a_live_parent_invocation_and_its_capability() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("research");
    let parent = plugin.for_invocation(false, CancellationToken::new());
    let child = plugin.for_invocation(true, CancellationToken::new());
    assert_eq!(
        call(plugin.user_data() as usize, json!({"op":"start"})).0,
        1
    );
    assert_eq!(
        call(parent.user_data() as usize, json!({"op":"start"})).0,
        1
    );
    assert_eq!(
        call(
            parent.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":"guessed"})
        )
        .0,
        1
    );
    assert_eq!(
        call(
            child.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":child.invocation_key()})
        )
        .0,
        1
    );
    // A child synchronous callback cannot spoof another invocation's ID/key.
    {
        let _guard = child.enter_invocation();
        assert_eq!(
            call(
                parent.user_data() as usize,
                json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()})
            )
            .0,
            1
        );
    }
    let accepted = call(
        parent.user_data() as usize,
        json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()}),
    );
    assert_eq!(accepted.0, 0);
    assert_eq!(
        accepted.1["owner"],
        plugin.plugin_identity.as_ref().unwrap().0
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    parent.invocation.as_ref().unwrap().2.cancel();
    assert_eq!(
        call(
            parent.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()})
        )
        .0,
        1
    );
}

#[tokio::test]
async fn dropped_candidate_and_unknown_handles_are_rejected_without_pointer_dereference() {
    let host = Arc::new(Host::default());
    let old = ActionBridge::new(Handle::current(), host.clone());
    let old_id = old.user_data() as usize;
    let candidate = ActionBridge::new(Handle::current(), host);
    let candidate_id = candidate.user_data() as usize;
    drop(candidate); // Failed reload: old bridge is still active.
    assert!(ActionBridge::resolve_handle(old_id).is_some());
    assert!(ActionBridge::resolve_handle(candidate_id).is_none());
    assert_eq!(
        call(candidate_id, json!({})).1["error"],
        "expired runtime handle"
    );
    assert_eq!(
        call(usize::MAX, json!({})).1["error"],
        "expired runtime handle"
    );
    let replacement = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    assert_ne!(replacement.user_data() as usize, candidate_id);
}

#[tokio::test]
async fn owner_scan_does_not_upgrade_or_pin_unrelated_bridges() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("retiring");
    let invocation = plugin.for_invocation(false, CancellationToken::new());
    let own = [&root, &plugin, &invocation];
    for bridge in own {
        bridge.user_data();
        assert_eq!(Arc::strong_count(bridge), 1);
    }

    let unrelated = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let unrelated_plugin = unrelated.for_plugin("unrelated");
    let unrelated_invocation = unrelated_plugin.for_invocation(false, CancellationToken::new());
    let unrelated_ids: Vec<_> = [&unrelated, &unrelated_plugin, &unrelated_invocation]
        .into_iter()
        .map(|bridge| bridge.user_data() as usize)
        .collect();

    // Hold the exact scan used by invalidate at the former race window. The
    // global upgrade-before-owner-filter pinned other sessions' invocations
    // until this vector was dropped, delaying immediate handle retirement.
    let live = root.bridges_for_owner();
    assert_eq!(live.len(), own.len());
    assert!(live
        .iter()
        .all(|bridge| Arc::ptr_eq(&bridge.active, &root.active)));
    for bridge in own {
        assert_eq!(Arc::strong_count(bridge), 2);
    }
    for bridge in [&unrelated, &unrelated_plugin, &unrelated_invocation] {
        assert_eq!(Arc::strong_count(bridge), 1);
    }
    drop(unrelated_invocation);
    drop(unrelated_plugin);
    drop(unrelated);
    for id in unrelated_ids {
        assert!(ActionBridge::resolve_handle(id).is_none());
    }
    drop(live);
    for bridge in own {
        assert_eq!(Arc::strong_count(bridge), 1);
    }
}

#[tokio::test]
async fn finishing_an_invocation_does_not_unload_its_plugin_owner() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("research");
    let invocation = plugin.for_invocation(false, CancellationToken::new());
    let id = invocation.user_data() as usize;
    drop(invocation);
    assert!(ActionBridge::resolve_handle(id).is_none());
    assert_eq!(host.revoked.load(Ordering::SeqCst), 0);
    root.invalidate();
    assert_eq!(host.revoked.load(Ordering::SeqCst), 1);
}

mod cancel_wait;
mod cleanup;
mod driver;
mod pacing;
mod streaming;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_questions_use_plugin_owner_and_stale_sinks_cannot_resurrect_panels() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("btw");
    let invoke = |bridge: &Arc<ActionBridge>| {
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::SideQuestion.into(),
            StbStringRef::from_str(r#"{"op":"start","question":"q","owner":999}"#),
            &mut out,
            bridge.user_data(),
        );
        // SAFETY: trampoline transfers its output allocation to this caller.
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        };
        (
            rc,
            serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap(),
        )
    };
    assert_eq!(invoke(&root).0, 1);
    let invocation = plugin.for_invocation(false, CancellationToken::new());
    assert_eq!(invoke(&invocation).0, 1);
    let value = invoke(&plugin);
    assert_eq!(value.0, 0);
    assert_eq!(value.1["owner"], plugin.plugin_identity.as_ref().unwrap().0);
    assert!(!root.extension_status_mailbox().panels().is_empty());
    root.invalidate();
    assert!(root.extension_status_mailbox().panels().is_empty());
    let sink = host.side_sinks.lock().unwrap()[0].clone();
    assert!(sink(json!({"op":"register","id":"btw-test","type":"text"})).is_err());
    assert!(root.extension_status_mailbox().panels().is_empty());
}

/// A newly appended action id must also bump the trampoline's `max_action_id`,
/// or it dispatches to `2` (unknown id) instead of reaching the host.
///
/// This pins the highest id end-to-end through the FFI entrypoint, which is the
/// one place the enum value, the `TryFrom` table and the ceiling have to agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_reaches_the_host_and_is_not_blocked_by_the_action_id_ceiling() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("stopper");
    let mut out = StbString::empty();
    let rc = trampoline_runtime_action(
        RuntimeActionId::Abort.into(),
        StbStringRef::from_str("{}"),
        &mut out,
        plugin.user_data(),
    );
    // SAFETY: the trampoline returned a live host allocation to its caller.
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
    };
    let value: Value = serde_json::from_str(&owned.to_string_lossy()).unwrap();
    assert_eq!(rc, 0, "Abort must dispatch, not fail as an unknown id");
    assert_eq!(value, json!({"aborted": false}));
    assert_eq!(host.aborts.load(Ordering::SeqCst), 1);
}

/// Independently developed wire actions must survive integration together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrated_commands_abort_dialog_and_stop_round_dispatch_without_renumbering() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("integrated-actions");
    for (id, expected) in [
        (
            26,
            json!({"commands":[{"name":"integrated","description":"both actions coexist","source":"extension"}]}),
        ),
        (27, json!({"aborted":false})),
        (28, json!({"requests":[]})),
        (29, json!({"requested":true})),
    ] {
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            id,
            StbStringRef::from_str("{}"),
            &mut out,
            plugin.user_data(),
        );
        assert_eq!(rc, 0, "action {id} must reach its own host method");
        // SAFETY: a successful trampoline call returns a live host allocation.
        let owned = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        };
        let value: Value = serde_json::from_str(&owned.to_string_lossy()).unwrap();
        assert_eq!(value, expected);
    }
    assert!(root.ui_dialog_mailbox().external_takeover());
    root.ui_dialog_mailbox().release_takeover();
    assert!(!root.ui_dialog_mailbox().external_takeover());
    assert_eq!(host.command_lists.load(Ordering::SeqCst), 1);
    assert_eq!(host.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(host.stop_rounds.load(Ordering::SeqCst), 1);
    let mut out = StbString::empty();
    assert_eq!(
        trampoline_runtime_action(
            RuntimeActionId::MAX + 1,
            StbStringRef::from_str("{}"),
            &mut out,
            plugin.user_data()
        ),
        2
    );
    assert_eq!(host.command_lists.load(Ordering::SeqCst), 1);
    assert_eq!(host.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(host.stop_rounds.load(Ordering::SeqCst), 1);
}

#[path = "../../../rpi-agent/tests/common/mod.rs"]
mod agent_common;
mod tool_program;
