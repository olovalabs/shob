//! A failed settled entry must not be overtaken by later transcript messages.
use super::*;
use rpi_ai::providers::faux::{faux_tool_call, FauxStep};

async fn messages(storage: &dyn SessionStorage) -> Vec<MessageEntry> {
    storage
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Message(message) => Some(message),
            _ => None,
        })
        .collect()
}

async fn assert_sequence(storage: &dyn SessionStorage, roles: &[&str]) {
    let entries = messages(storage).await;
    let actual: Vec<_> = entries
        .iter()
        .map(|entry| match &entry.message {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "result",
            _ => "custom",
        })
        .collect();
    assert_eq!(actual, roles, "branch must retain loop message order");
    assert_valid(storage).await;
    for record in storage
        .find_records(&RecordQuery {
            record_type: Some("tool_started"),
            ..Default::default()
        })
        .await
        .unwrap()
    {
        let LaneRecord::ToolStarted(record) = record else {
            unreachable!()
        };
        let assistant = entries
            .iter()
            .position(|entry| entry.base.id == record.assistant_entry_id)
            .unwrap();
        let result = entries
            .iter()
            .position(|entry| entry.base.id == record.result_entry_id)
            .unwrap();
        assert!(result > assistant);
        assert!(
            entries[assistant + 1..result]
                .iter()
                .all(|entry| matches!(entry.message, AgentMessage::ToolResult(_))),
            "a result cannot cross a subsequent assistant/context boundary"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_result_is_durable_before_the_next_provider_and_final_assistant() {
    let (harness, storage, tool, provider) = scripted_tool_fixture(
        AgentHarnessOptions::default().session.storage(),
        1,
        false,
        false,
        FauxScript::new()
            .with_tool_call("count", serde_json::json!({}))
            .with_text("final"),
    )
    .await;
    let seen = Arc::new(Mutex::new(None));
    let observed = seen.clone();
    let backing = storage.clone();
    provider.script().set_responses(vec![
        FauxStep::tool_call("count", serde_json::json!({})),
        FauxStep::factory(move |_| {
            let entries = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(messages(backing.as_ref()))
            });
            *observed.lock().unwrap() = Some(
                entries
                    .iter()
                    .filter(|entry| matches!(entry.message, AgentMessage::ToolResult(_)))
                    .count(),
            );
            faux_assistant_message("final", StopReason::Stop)
        }),
    ]);
    let result = harness.prompt_text("run", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        *seen.lock().unwrap(),
        Some(1),
        "next provider must not cross an uncommitted result"
    );
    assert_sequence(
        storage.as_ref(),
        &["user", "assistant", "result", "assistant"],
    )
    .await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_failed_result_cannot_be_overtaken_by_a_later_result_in_the_batch() {
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant.content = vec![
        faux_tool_call("count", serde_json::json!({})),
        faux_tool_call("count", serde_json::json!({})),
    ];
    let call_ids: Vec<_> = assistant
        .content
        .iter()
        .map(|content| match content {
            rpi_ai::types::Content::ToolCall(call) => call.id.clone(),
            _ => unreachable!(),
        })
        .collect();
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("final")]);
    let (harness, storage, tool, _) = scripted_tool_fixture(
        AgentHarnessOptions::default().session.storage(),
        1,
        false,
        false,
        script,
    )
    .await;
    harness.prompt_text("run", vec![]).await.unwrap();
    let entries = messages(storage.as_ref()).await;
    let results: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.message {
            AgentMessage::ToolResult(result) => Some(result.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results, call_ids,
        "result commits must preserve source order even after a write error"
    );
    assert_sequence(
        storage.as_ref(),
        &["user", "assistant", "result", "result", "assistant"],
    )
    .await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_tool_rounds_commit_each_result_before_the_next_assistant() {
    let (harness, storage, tool, _) = scripted_tool_fixture(
        AgentHarnessOptions::default().session.storage(),
        1,
        false,
        false,
        FauxScript::new()
            .with_tool_call("count", serde_json::json!({}))
            .with_tool_call("count", serde_json::json!({}))
            .with_text("final"),
    )
    .await;
    harness.prompt_text("run", vec![]).await.unwrap();
    assert_sequence(
        storage.as_ref(),
        &[
            "user",
            "assistant",
            "result",
            "assistant",
            "result",
            "assistant",
        ],
    )
    .await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_prefix_retry_keeps_the_run_open_before_another_provider_request() {
    let (harness, storage, tool, provider) = scripted_tool_fixture(
        AgentHarnessOptions::default().session.storage(),
        2,
        false,
        false,
        FauxScript::new()
            .with_tool_call("count", serde_json::json!({}))
            .with_text("must not be requested"),
    )
    .await;
    let result = harness.prompt_text("run", vec![]).await;
    assert!(result.is_err(), "persistent write failure must propagate");
    assert_eq!(
        provider.state().call_count.load(Ordering::SeqCst),
        1,
        "no provider call may overtake a failed prefix retry"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert!(records(&storage, "operation_finished").await.is_empty());
    assert_eq!(
        storage
            .find_open_operations("main", None)
            .await
            .unwrap()
            .len(),
        1
    );
    let id = started(storage.as_ref()).await.result_entry_id;
    assert_eq!(
        *storage.result_attempts.lock().unwrap(),
        vec![id.clone(), id]
    );
    let entries = messages(storage.as_ref()).await;
    assert_eq!(
        entries.len(),
        2,
        "only user + tool-call assistant can be durable"
    );
    assert_valid(storage.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_repairs_the_durable_batch_before_appending_a_newer_stream() {
    let (harness, storage, tool, _) = scripted_tool_fixture(
        AgentHarnessOptions::default().session.storage(),
        2,
        false,
        false,
        FauxScript::new().with_tool_call("count", serde_json::json!({})),
    )
    .await;
    assert!(harness.prompt_text("run", vec![]).await.is_err());
    let run = storage.find_open_operations("main", None).await.unwrap()[0]
        .base
        .id
        .clone();
    // Independently publish a later stream so the recovery-order regression is
    // deterministic, even if the cancellation-generated abort has not flushed.
    let partial = Arc::new(faux_assistant_message("newer partial", StopReason::Pending));
    let events = [
        rpi_ai::types::AssistantMessageEvent::Start {
            partial: partial.clone(),
        },
        rpi_ai::types::AssistantMessageEvent::TextStart {
            content_index: 0,
            partial,
        },
    ];
    let mut encoder = rpi_ai::frames::AssistantMessageFrameEncoder::new();
    let frames = events
        .iter()
        .enumerate()
        .map(|(index, event)| AssistantFrameRecord {
            base: RecordBase {
                id: format!("later-frame-{index}"),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            run_id: run.clone(),
            stream_index: 100,
            op: AssistantFrameOp::Append,
            frame: Some(serde_json::to_value(encoder.encode(event).unwrap().unwrap()).unwrap()),
        })
        .collect();
    storage.append_progress(frames).await.unwrap();
    drop(harness);
    let mut recovered_id = None;
    for _ in 0..2 {
        let restored = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(storage.clone(), None),
            tools: vec![HarnessTool::new(tool.clone())],
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let result = assert_bound_result(storage.as_ref()).await;
        let entries = messages(storage.as_ref()).await;
        let newer: Vec<_> = entries
            .iter()
            .filter(|entry| {
                matches!(&entry.message, AgentMessage::Assistant(assistant)
                if assistant.content.iter().any(|content| matches!(content,
                    rpi_ai::types::Content::Text(text) if text.text == "newer partial")))
            })
            .collect();
        assert_eq!(newer.len(), 1, "recover the later stream exactly once");
        assert!(result.base.seq < newer[0].base.seq);
        if let Some(id) = &recovered_id {
            assert_eq!(&result.base.id, id);
        }
        recovered_id = Some(result.base.id);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        drop(restored);
    }
}
