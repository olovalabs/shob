//! M5g integration — full harness run over a faux provider + real
//! `read`/`write`/`bash` tools against an `InMemoryExecutionEnv`. Mirrors
//! `packages/agent/test/harness/agent-harness.test.ts` "full run" cases at the
//! integration level the Rust port targets.
//!
//! This is the capstone M5 test: it drives the complete
//! `AgentLane::prompt_text → run_core → run_agent_loop → persist → outcome`
//! path end-to-end with NO real LLM, asserting:
//! - the run resolves `HarnessRunOutcome::Completed`;
//! - the faux provider's scripted tool call actually executes against the
//!   in-memory env (the file the model "asked to write" persists in the
//!   session, and the tool result message round-trips back to the model for a
//!   final text turn);
//! - the durable session gained the expected entry chain
//!   (user → assistant-with-tool-call → tool-result → assistant-final);
//! - an `operation_started` + `operation_finished` record pair is on the lane,
//!   and the finished outcome is `Completed`;
//! - the `RunStart`/`RunEnd` events fire on the harness bus with the run id;
//! - the defensive-copy contract holds post-run (`get_tools` / `get_active_tools`
//!   still return clones).
//!
//! Mechanics worth knowing:
//! - The harness's `build_stream_fn` bridges the sync `StreamFn` contract to
//!   async `Provider::stream_simple` via `block_in_place` + `block_on` — this
//!   needs a multi-threaded runtime, hence `flavor = "multi_thread"`.
//! - `AgentHarnessOptions` wires a faux provider as `models`, a fresh
//!   `InMemorySessionStorage` as `session`, and the three built-in tools from a
//!   shared `ExecutionToolContext`.
//! - The faux script's *last* empty-queue step would emit `Error` if the loop
//!   called the provider a 3rd time; the second step (`Stop`/`StopReason::Stop`)
//!   has no tool calls so the loop terminates there — no 3rd call happens.

use std::sync::{Arc, Mutex};

use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage, CollectorEmitter};
use rpi_ai::providers::faux::{
    faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
};
use rpi_ai::types::{UserContent, UserMessage};
use rpi_ai::Provider;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery, LaneRecord, RecordQuery};
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, RetryPolicy, ToolReplay};
use rpi_tools::{
    create_bash_tool, create_read_tool, create_write_tool, ExecutionToolContext, FileSystem,
    InMemoryExecutionEnv, MutationQueueRegistry,
};

// `RPI_MAX_TURNS_PER_RUN` is process-global, so tests that change it must not
// overlap. The guard stays held across the async harness run, not just the
// set/remove call, because the harness reads the variable while starting a run.
static RUN_BUDGET_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pending_frames(session: &Session) -> Vec<LaneRecord> {
    let runs = session
        .find_records(&RecordQuery {
            record_type: Some("operation_started"),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut frames = Vec::new();
    for run in runs {
        frames.extend(
            session
                .read_progress(run.id())
                .await
                .unwrap()
                .into_iter()
                // CommitStream binds identity, not content. Crash gates must
                // wait for actual streamed frames, as on main.
                .filter(|frame| frame.op == rpi_harness::session::types::AssistantFrameOp::Append)
                .map(LaneRecord::AssistantFrame),
        );
    }
    frames
}
#[path = "harness_run_e2e/native_identity_blockers.rs"]
mod native_identity_blockers;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declined_compaction_records_a_terminal_outcome() {
    let (harness, _, _) = harness_with(FauxScript::new().with_text("unused")).await;
    let result = harness.compact(None).await.unwrap();
    assert!(matches!(
        result.outcome,
        rpi_harness::agent_harness::CompactionOutcome::Declined { .. }
    ));
    let records = harness
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    let terminal_records = records.iter().filter(|record| {
        matches!(record, LaneRecord::OperationFinished(finished)
            if finished.run_id == result.run_id
                && finished.outcome == rpi_harness::session::types::OperationOutcome::Declined)
    });
    assert_eq!(terminal_records.count(), 1);
}

/// Build an `AgentHarness` over a faux provider + in-memory env + the read/
/// write/bash tools. The faux script is caller-supplied so each test pins the
/// exact LLM turns. Returns `(harness, provider, env)` so the test can inspect
/// provider call count + the in-memory FS after the run.
async fn harness_with(
    script: FauxScript,
) -> (AgentHarness, Arc<FauxProvider>, Arc<InMemoryExecutionEnv>) {
    harness_with_options(script, RetryPolicy::default(), None).await
}

async fn harness_with_options(
    script: FauxScript,
    retry: RetryPolicy,
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
) -> (AgentHarness, Arc<FauxProvider>, Arc<InMemoryExecutionEnv>) {
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();

    // In-memory execution env + tool context. `InMemoryExecutionEnv` implements
    // both `ExecutionEnv` and `MutatingEnv`.
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let read = create_read_tool(&ctx, None);
    let write = create_write_tool(&ctx);
    let bash = create_bash_tool(&ctx, None);
    let tools: Vec<HarnessTool> = vec![read, write, bash]
        .into_iter()
        .map(HarnessTool::new)
        .collect();
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    // Fresh in-memory session storage (no prior records — `create` would
    // otherwise reject via the restore gate).
    let metadata = SessionMetadata {
        id: "e2e".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);

    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry,
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: false,
    };

    let harness = AgentHarness::create(options).await.expect("create harness");
    (harness, provider, env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retryable_provider_failure_emits_retry_progress_and_recovers() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("Recovered after retry."),
    ]);

    let (collector, events) = CollectorEmitter::new();
    let retry = RetryPolicy {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 0,
        max_agent_delay_ms: 0,
    };
    let (harness, provider, _) =
        harness_with_options(script, retry, Some(Arc::new(collector))).await;

    let result = harness
        .prompt_text("retry this request", vec![])
        .await
        .expect("retrying prompt completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled {
            attempt: 1,
            max_retries: 2,
            delay_ms: 0,
            error,
        } if error == "503 service unavailable"
    )));
}

/// Collect bus events into a shared vec while the closure owns a `Handle`.
fn record_runtime_events(harness: &AgentHarness) -> Arc<Mutex<Vec<HarnessEvent>>> {
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_listener = collected.clone();
    // `on` registers a direct listener; we keep the `OnUnsubscribe` guard alive
    // for the duration of the run by returning both. But we only need the vec;
    // drop the guard to keep it simple — events during the run still land
    // because the harness emits synchronously inline on the calling task.
    let _off = harness
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |_e| {
            // no-op placeholder; we use the all-events watch below
        });
    // Use the watch surface (snapshot now → buffer → start) to capture every
    // event including RunStart, which fires *during* `prompt_text` before a
    // live listener could be installed post-call.
    let mut watch = harness.events().watch(|| ());
    let collected_for_watch = collected_for_listener;
    watch.start(Arc::new(move |event: &HarnessEvent| {
        collected_for_watch.lock().unwrap().push(event.clone());
    }));
    // Prevent the watch from being dropped (which would NOT unsubscribe — the
    // bus keeps it — but we want the listener alive for the whole test).
    std::mem::forget(watch);
    collected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_turn_carries_the_session_id_for_prompt_caching() {
    let script = FauxScript::new().with_tool_call("bash", serde_json::json!({ "command": "true" }));
    script.append_responses(vec![FauxStep::text("done")]);
    let (harness, provider, _) = harness_with(script).await;

    harness
        .prompt_text("run it", vec![])
        .await
        .expect("prompt completes");

    let ids = provider.state().session_ids.lock().unwrap().clone();
    assert_eq!(ids, vec![Some("e2e".to_string()); 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_run_with_write_tool_call_completes_and_persists() {
    // Script:
    //   turn 1 — assistant emits a `write` tool call
    //   turn 2 — (after tool result) assistant emits a final text turn, Stop
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "out.txt", "content": "hello from harness e2e" }),
        )
        .with_text("Done — I wrote the file.");
    let (harness, provider, env) = harness_with(script).await;

    let events = record_runtime_events(&harness);

    let result = harness
        .prompt_text("Write 'hello from harness e2e' to out.txt.", vec![])
        .await
        .expect("prompt_text completes");
    let run_id = result.run_id.clone();
    match &result.outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            assert!(!leaf_id.is_empty(), "leaf_id must be set on Completed");
            assert!(!final_entry_id.is_empty(), "final_entry_id must be set");
            // The final assistant message is the "Done — ..." text turn (no tool calls).
            let has_text = final_message
                .content
                .iter()
                .any(|c| matches!(c, rpi_ai::types::Content::Text(_)));
            assert!(has_text, "final message should carry text content");
            assert_eq!(final_message.stop_reason, rpi_ai::types::StopReason::Stop);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // The faux provider was called once per turn = 2 calls.
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "faux provider should have been called for both turns"
    );

    // The write tool actually wrote the file in the in-memory env.
    let written = env
        .read_text_file("/out.txt", None)
        .await
        .expect("out.txt was written by the write tool via the harness run");
    // `InMemoryExecutionEnv` resolves a bare "out.txt" against cwd "/", so the
    // absolute path is "/out.txt".
    assert_eq!(written, "hello from harness e2e");

    // ---- durable session assertions ----------------------------------------

    // The branch path should contain (oldest→newest):
    //   user prompt | assistant(tool_call) | tool_result | assistant(final text)
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .expect("leaf")
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    assert!(
        path.len() >= 4,
        "expected at least 4 entries (user, assistant, toolresult, assistant-final); got {}",
        path.len()
    );
    // Sanity: the first entry is the user prompt.
    assert!(matches!(
        path[0],
        rpi_harness::session::types::Entry::Message(_)
    ));
    // And the leaf entry (last) is the final assistant text turn.
    let last = path.last().unwrap();
    assert!(matches!(
        last,
        rpi_harness::session::types::Entry::Message(_)
    ));

    // ---- persisted tool-result timestamps are real wall-clock -------------
    //
    // Regression: `agent_loop::create_tool_result_message` stamped every tool
    // result from a bare atomic counter (`1, 2, 3, …`), so durable sessions
    // carried nonsense tool-result times (29, 38, 55, …) next to real assistant
    // times. Nothing asserted on them, so it survived.
    //
    // Only the tool result is checked here: its timestamp is produced by the
    // shared agent loop, i.e. for every provider. Assistant timestamps come
    // from the provider, and the faux test double deliberately uses a counter
    // (see the note on `providers::anthropic::now_ms`).
    let mut tool_result_timestamps = Vec::new();
    for entry in &path {
        let rpi_harness::session::types::Entry::Message(message_entry) = entry else {
            continue;
        };
        if let rpi_agent::AgentMessage::ToolResult(t) = &message_entry.message {
            tool_result_timestamps.push(t.timestamp);
        }
    }
    assert!(
        !tool_result_timestamps.is_empty(),
        "expected a persisted toolResult to check its timestamp"
    );
    for timestamp in tool_result_timestamps {
        assert!(
            timestamp > 1_600_000_000_000,
            "toolResult timestamp {timestamp} is not epoch milliseconds \
             (the agent loop used to stamp tool results from a 1,2,3… counter)"
        );
    }

    // An operation_started (intent Run) + operation_finished (Completed) record
    // pair should be present on the lane, with matching run_id.
    let started = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some("operation_started"),
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
        .expect("find operation_started");
    assert_eq!(
        started.len(),
        1,
        "exactly one operation_started for this run_id"
    );
    let finished = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
        .expect("find operation_finished");
    assert_eq!(
        finished.len(),
        1,
        "exactly one operation_finished for this run_id"
    );
    if let LaneRecord::OperationFinished(f) = &finished[0] {
        assert_eq!(
            f.outcome,
            rpi_harness::session::types::OperationOutcome::Completed
        );
    } else {
        panic!("expected OperationFinished record");
    }

    // ---- harness bus events ------------------------------------------------

    let evs = events.lock().unwrap();
    let run_starts = evs
        .iter()
        .filter(|e| matches!(e, HarnessEvent::RunStart(_)))
        .count();
    let run_ends = evs
        .iter()
        .filter(|e| matches!(e, HarnessEvent::RunEnd(_)))
        .count();
    assert_eq!(run_starts, 1, "exactly one RunStart");
    assert_eq!(run_ends, 1, "exactly one RunEnd");
    // The RunEnd outcome is Completed.
    let run_end_outcome = evs
        .iter()
        .find_map(|e| match e {
            HarnessEvent::RunEnd(re) => Some(re.outcome),
            _ => None,
        })
        .expect("a RunEnd event");
    assert_eq!(run_end_outcome, RunEndOutcome::Completed);

    // ---- defensive-copy contract still holds post-run ----------------------

    let tools = harness.get_tools().await.expect("get_tools after run");
    assert_eq!(tools.len(), 3, "tool registry intact after run");
    let active = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get_active_tools after run");
    assert_eq!(active.len(), 3, "active tools intact after run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_with_no_tool_calls_completes_in_single_turn() {
    // A simple text-only reply: no tool calls, so the loop terminates after one
    // turn. Asserts the happy-path single-turn shape (used widely in tests).
    let script = FauxScript::new().with_text("Hello back.");
    let (harness, provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("Say hello.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );
    // One turn → one provider call.
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
    );
    // Two persisted entries: the user prompt + the assistant reply.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .unwrap()
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(path.len(), 2, "user + assistant reply only");
}

// ---------------------------------------------------------------------------
// B3b: AgentHarnessOptions forwards before_tool_call / after_tool_call /
// transform_context into the live AgentLoopConfig (previously these existed on
// AgentLoopConfig but the harness hardcoded them to None). This test is the
// harness-level proof: set after_tool_call on the options, run a tool-call
// script, and assert the persisted ToolResultMessage carries the patched usage
// — i.e. the hook the host installed via options actually ran inside the loop.
//
// (The pi-agent before_after_hooks tests prove the loop itself honors the hooks
// at the unit level; this proves the harness WIRING from options → inner →
// snapshot_config → AgentLoopConfig is live, which is the B3b deliverable.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_forwards_after_tool_call_option_into_loop() {
    use rpi_agent::{AfterToolCall, AfterToolCallResult};
    use rpi_ai::types::{Usage, UsageCost};

    let patched = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 26,
        cost: UsageCost {
            input: 0.5,
            output: 0.6,
            cache_read: 0.7,
            cache_write: 0.8,
            total: 2.6,
        },
    };
    let patched_for_hook = patched.clone();

    let fired: Arc<std::sync::Mutex<bool>> = Arc::new(std::sync::Mutex::new(false));
    let after: AfterToolCall = {
        let fired = Arc::clone(&fired);
        let patched = patched_for_hook.clone();
        Arc::new(move |_ctx: rpi_agent::AfterToolCallContext<'_>, _signal| {
            let fired = Arc::clone(&fired);
            let patched = patched.clone();
            Box::pin(async move {
                *fired.lock().unwrap() = true;
                Some(AfterToolCallResult {
                    usage: Some(patched),
                    ..AfterToolCallResult::default()
                })
            })
        })
    };

    // Script: turn 1 a `write` tool call, turn 2 a final text turn.
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "b3b.txt", "content": "patched-usage" }),
        )
        .with_text("done");
    let (harness, _provider, _env) = {
        // Build a harness that carries the after_tool_call option. We reuse
        // `harness_with` for everything else, then rebuild options with the
        // hook attached. Easiest: construct a second harness via the same path
        // but we need the options — so replicate the minimal build inline.
        let provider = FauxProvider::new(script);
        let model = provider.default_model().clone();
        let env = Arc::new(InMemoryExecutionEnv::new());
        let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
        let _ = Arc::new(MutationQueueRegistry::new());
        let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let write = create_write_tool(&ctx);
        let tools: Vec<HarnessTool> = vec![write].into_iter().map(HarnessTool::new).collect();
        let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
        let metadata = SessionMetadata {
            id: "b3b-after".into(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = Arc::new(InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let session = Session::new(storage, None);
        let options = AgentHarnessOptions {
            model,
            thinking_level: Default::default(),
            active_tool_names: active,
            tools,
            system_prompt: None,
            resources: Default::default(),
            stream_options: Default::default(),
            retry: Default::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: Default::default(),
            drive: Default::default(),
            session,
            models: vec![provider.clone() as Arc<dyn Provider>],
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: Some(after),
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
            allow_existing_session: false,
        };
        let harness = AgentHarness::create(options).await.expect("create harness");
        (harness, provider, env)
    };

    let result = harness
        .prompt_text("Write patched-usage to b3b.txt.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );

    // The hook fired (the harness forwarded options.after_tool_call into the
    // live AgentLoopConfig, so the loop invoked it).
    assert!(
        *fired.lock().unwrap(),
        "after_tool_call hook installed via AgentHarnessOptions must fire inside the loop"
    );

    // And the persisted ToolResultMessage carries the patched usage — the
    // override the hook returned was applied by the loop and survived the
    // harness's persist path. This is the end-to-end B3b proof.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .unwrap()
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    let tool_result = path.iter().find_map(|e| match e {
        rpi_harness::session::types::Entry::Message(m) => match &m.message {
            rpi_agent::AgentMessage::ToolResult(t) => Some(t.clone()),
            _ => None,
        },
        _ => None,
    });
    let tool_result = tool_result.expect("a toolResult message persisted");
    assert_eq!(
        tool_result.usage,
        Some(patched),
        "persisted ToolResultMessage.usage must be the after_tool_call override"
    );
}

// ---------------------------------------------------------------------------
// B4: ProviderHooks before_request patches the LIVE per-call SimpleStreamOptions.
// The harness build_stream_fn closure fires the hook before each stream_simple
// call and applies the returned SimpleStreamOptionsPatch to a clone of opts.
// The FauxProvider surfaces the opts.session_id it received on the assistant
// message... but faux ignores most opt fields. The clean observable is the
// `metadata`/`headers` route: faux carries `session_id` from opts onto its
// faux-cache-hit path, but simplest is to assert the hook FIRED with the model
// the loop is about to call (the per-call bridge ran). We pair the hook with a
// counter and assert call_count == hook fires (one per stream_simple call).
// (after_response observation is wired on the emitter side in B3+; here we
// prove before_request runs per-call, which is the load-bearing B4 fix.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_fires_provider_hooks_before_request_per_call() {
    use rpi_ai::{ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};

    // One hook fire per stream_simple call. The script has 2 turns
    // (tool-call + final text), so we expect 2 provider calls == 2 hook fires.
    let fires: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    struct CountingHooks {
        fires: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl ProviderHooks for CountingHooks {
        fn before_request(
            &self,
            model: &rpi_ai::Model,
            _ctx: &rpi_ai::types::Context,
            _opts: &SimpleStreamOptions,
        ) -> Option<SimpleStreamOptionsPatch> {
            self.fires.lock().unwrap().push(model.id.clone());
            // Return a no-op patch (None would also work; exercise the apply path).
            None
        }
    }

    let hooks: Arc<dyn ProviderHooks> = Arc::new(CountingHooks {
        fires: Arc::clone(&fires),
    });

    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "b4.txt", "content": "hook" }),
        )
        .with_text("done");
    // Reuse the harness build, then rebuild options with the provider hook.
    let (harness, provider, _env) = {
        let provider = FauxProvider::new(script);
        let model = provider.default_model().clone();
        let env = Arc::new(InMemoryExecutionEnv::new());
        let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
        let _ = Arc::new(MutationQueueRegistry::new());
        let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let write = create_write_tool(&ctx);
        let tools: Vec<HarnessTool> = vec![write].into_iter().map(HarnessTool::new).collect();
        let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
        let metadata = SessionMetadata {
            id: "b4-hooks".into(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = Arc::new(InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let session = Session::new(storage, None);
        let options = AgentHarnessOptions {
            model,
            thinking_level: Default::default(),
            active_tool_names: active,
            tools,
            system_prompt: None,
            resources: Default::default(),
            stream_options: Default::default(),
            retry: Default::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: Default::default(),
            drive: Default::default(),
            session,
            models: vec![provider.clone() as Arc<dyn Provider>],
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: Some(hooks),
            allow_existing_session: false,
        };
        let harness = AgentHarness::create(options).await.expect("create harness");
        (harness, provider, env)
    };

    let result = harness
        .prompt_text("Write hook to b4.txt.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );

    // Two turn script → two provider calls; before_request fired once each.
    let provider_calls = provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let hook_fires = fires.lock().unwrap().clone();
    assert_eq!(provider_calls, 2, "two-turn script = two provider calls");
    assert_eq!(
        hook_fires.len(),
        2,
        "before_request must fire once per stream_simple call (per-call, not run-once)"
    );
}

/// `ProviderHooks::transform_context` must reach the provider request: the
/// prompt a hook installs is the prompt the model is asked with.
///
/// This is the load-bearing half of the pi `transform_context` parity work. The
/// bridge test proves a plugin's text comes back through the ABI; this one
/// proves the harness then applies it to the LIVE `Context` handed to
/// `stream_simple` — and that the base prompt the hook received is still there
/// underneath, so nothing downstream silently dropped it.
///
/// Uses a recording provider rather than the faux one because the assertion is
/// about what the *provider* was handed, not what the loop did with the reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_hooks_can_replace_the_system_prompt_the_model_sees() {
    use rpi_ai::types::Context as AiContext;
    use rpi_ai::{AssistantMessageEventStream, ContextPatch, ProviderHooks};

    /// Wraps a faux provider and records the system prompt of every request.
    struct RecordingProvider {
        inner: Arc<FauxProvider>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn models(&self) -> &[rpi_ai::Model] {
            self.inner.models()
        }
        async fn stream_simple(
            &self,
            model: &rpi_ai::Model,
            ctx: &AiContext,
            opts: &rpi_ai::SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.seen.lock().unwrap().push(ctx.system_prompt.clone());
            self.inner.stream_simple(model, ctx, opts).await
        }
    }

    /// Replaces the prompt with `<marker>\n<original>` — an append built the way
    /// pi requires (read the event's prompt, return the concatenation).
    struct AppendHooks;
    impl ProviderHooks for AppendHooks {
        fn transform_context(
            &self,
            _model: &rpi_ai::Model,
            ctx: &AiContext,
        ) -> Option<ContextPatch> {
            Some(ContextPatch {
                system_prompt: Some(format!(
                    "HOOK-FIRST\n{}",
                    ctx.system_prompt.as_deref().unwrap_or_default()
                )),
            })
        }
    }

    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider {
        inner: FauxProvider::new(FauxScript::new().with_text("done")),
        seen: Arc::clone(&seen),
    });

    let model = provider.inner.default_model().clone();

    // Same shape as the B4 provider-hooks test above: build options directly so
    // this test can set both the hooks AND a system prompt.
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let tool_ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let tools: Vec<HarnessTool> = vec![create_write_tool(&tool_ctx)]
        .into_iter()
        .map(HarnessTool::new)
        .collect();
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
    let metadata = SessionMetadata {
        id: "b4-transform-context".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        // The composed prompt a real session would have built; the hook must
        // receive this and pass it through in its replacement.
        system_prompt: Some("BASE SYSTEM PROMPT".to_string()),
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: Some(Arc::new(AppendHooks) as Arc<dyn ProviderHooks>),
        allow_existing_session: false,
    };
    let harness = AgentHarness::create(options).await.expect("create harness");

    harness
        .prompt_text("hello", vec![])
        .await
        .expect("prompt completes");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one-turn script = one provider call");
    let prompt = seen[0]
        .as_deref()
        .expect("the request must carry a system prompt — the harness had one");
    assert!(
        prompt.starts_with("HOOK-FIRST"),
        "the hook's text must be what the provider receives; got {prompt:?}"
    );
    assert!(
        prompt.contains("BASE SYSTEM PROMPT"),
        "the hook appended to the composed prompt, so the base must survive; \
         got {prompt:?}"
    );
}

/// Regression: the harness persists prompts itself and drives
/// `run_agent_loop` with an EMPTY prompts vec (to avoid double-counting them in
/// the provider context), so the loop never emits `message_start`/`message_end`
/// for the user's prompt.
///
/// The TUI renders user bubbles from `AgentEvent::MessageStart`, so this test
/// pins the contract the TUI must compensate for: a directly-sent prompt
/// produces NO user `MessageStart`, while a queued (steering) message DOES
/// (that one is drained inside the loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directly_sent_prompt_emits_no_user_message_start() {
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::text("Done.")]);
    let (collector, events) = CollectorEmitter::new();
    let (harness, _provider, _) =
        harness_with_options(script, RetryPolicy::default(), Some(Arc::new(collector))).await;

    harness
        .prompt_text("hello there", vec![])
        .await
        .expect("prompt completes");

    let events = events.lock().unwrap();
    let user_starts = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::MessageStart {
                    message: rpi_agent::AgentMessage::User(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        user_starts, 0,
        "harness passes an empty prompts vec, so the loop emits no user MessageStart; \
         the TUI must render the prompt itself. events: {events:#?}"
    );
    // The assistant still streams normally.
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageStart {
                message: rpi_agent::AgentMessage::Assistant(_),
                ..
            }
        )),
        "assistant MessageStart must still fire"
    );
}

/// The loop's only other exit is "the model stopped asking for tools", which
/// nothing bounds — one observed session ran 112 turns / 867 s in a single run
/// (`docs/debugging/llm-repetition-forensics.md` §二). The run budget is the backstop:
/// when a model keeps emitting tool calls forever, the run must end on a known
/// ceiling and say so, so a truncated task is never mistaken for a finished one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_budget_stops_a_looping_run_and_records_why() {
    let _env_lock = RUN_BUDGET_ENV_LOCK.lock().await;

    // A model that never stops asking for tools: far more scripted turns than
    // the ceiling, so only the guard can end this run.
    //
    // The guard is off by default (upstream parity), so this test opts in for
    // its duration. `std::env` is process-global; the lock above also prevents
    // the no-ceiling test below from changing it while the harness is running.
    let previous = std::env::var(rpi_harness::run_budget::MAX_TURNS_ENV).ok();
    std::env::set_var(
        rpi_harness::run_budget::MAX_TURNS_ENV,
        rpi_harness::run_budget::DEFAULT_MAX_TURNS_PER_RUN.to_string(),
    );

    let script = FauxScript::new();
    let step = || FauxStep::tool_call("bash", serde_json::json!({ "command": "echo tick" }));
    script.set_responses((0..200).map(|_| step()).collect());
    let (harness, provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("loop until the budget stops you", vec![])
        .await
        .expect("the run must terminate, not spin");

    // It terminated "Completed" (the loop's normal exit path) — which is exactly
    // why the reason has to be recorded separately.
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected a Completed outcome, got {:?}",
        result.outcome
    );

    let calls = provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let ceiling = rpi_harness::run_budget::DEFAULT_MAX_TURNS_PER_RUN as usize;
    assert_eq!(
        calls, ceiling,
        "the run must stop exactly at the turn ceiling, not run on"
    );

    // The stop reason is on the lane, rendered like any other custom message.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .expect("leaf")
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    let notices: Vec<String> = path
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message_entry) => {
                match &message_entry.message {
                    rpi_agent::AgentMessage::Custom(custom)
                        if custom.role == rpi_harness::messages::CUSTOM_ROLE =>
                    {
                        Some(
                            custom
                                .content
                                .iter()
                                .filter_map(|content| match content {
                                    rpi_ai::types::Content::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .filter(|text| text.contains("Stopped after"))
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "exactly one budget notice must be recorded; got {notices:?}"
    );

    // Restore the process-wide switch (see the opt-in above).
    match &previous {
        Some(value) => std::env::set_var(rpi_harness::run_budget::MAX_TURNS_ENV, value),
        None => std::env::remove_var(rpi_harness::run_budget::MAX_TURNS_ENV),
    }
    assert!(
        notices[0].contains(&ceiling.to_string()) && notices[0].contains("unfinished"),
        "the notice must name the budget and warn the work may be incomplete: {}",
        notices[0]
    );
}

/// Upstream parity: with no `RPI_MAX_TURNS_PER_RUN`, a run has **no** turn
/// ceiling — it ends only because the model stopped asking for tools. This is
/// the counterpart of the opt-in test above: it fails if anyone re-enables the
/// guard by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_turn_ceiling_by_default_matches_native_pi() {
    let _env_lock = RUN_BUDGET_ENV_LOCK.lock().await;

    let previous = std::env::var(rpi_harness::run_budget::MAX_TURNS_ENV).ok();
    std::env::remove_var(rpi_harness::run_budget::MAX_TURNS_ENV);

    // 199 tool-call turns, then a final plain answer so the loop's own exit
    // condition ("the model stopped asking for tools") is what ends the run.
    // That is 80 turns past the suggested ceiling, so an accidentally enabled
    // guard would truncate it and record a notice.
    let script = FauxScript::new();
    let step = || FauxStep::tool_call("bash", serde_json::json!({ "command": "echo tick" }));
    let mut steps: Vec<FauxStep> = (0..199).map(|_| step()).collect();
    steps.push(FauxStep::text("done"));
    script.set_responses(steps);
    let (harness, provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("run without a ceiling", vec![])
        .await
        .expect("the run must complete");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let calls = provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        calls,
        rpi_harness::run_budget::DEFAULT_MAX_TURNS_PER_RUN as usize + 80,
        "the default must not truncate a long run"
    );

    // No budget notice on the lane.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .expect("leaf")
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    // The budget notice is a `customType = "runBudget"` custom message; its
    // absence is what proves no ceiling fired.
    let notices: Vec<String> = path
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message_entry) => {
                match &message_entry.message {
                    rpi_agent::AgentMessage::Custom(custom)
                        if custom.role == rpi_harness::messages::CUSTOM_ROLE
                            && custom.data.get("customType").and_then(|v| v.as_str())
                                == Some("runBudget") =>
                    {
                        Some(custom.data.to_string())
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    assert!(
        notices.is_empty(),
        "an unbounded run must not record a budget notice: {notices:?}"
    );

    match &previous {
        Some(value) => std::env::set_var(rpi_harness::run_budget::MAX_TURNS_ENV, value),
        None => std::env::remove_var(rpi_harness::run_budget::MAX_TURNS_ENV),
    }
}

/// A crash mid-stream must not cost the whole run.
///
/// Before frame progress, a run persisted nothing until it finished, so killing
/// the process after minutes of work left only the user's prompt
/// (`docs/debugging/llm-repetition-forensics.md` §十一). Frames are appended to the record
/// stream as they arrive, so the committed prefix can be replayed.
///
/// The crash is simulated faithfully: the run future is dropped mid-stream by
/// aborting its task, which is exactly what process death does to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_run_can_be_salvaged_from_its_committed_frames() {
    // A deliberately slow stream: enough tokens that the run is still streaming
    // when we abort it, so the crash lands mid-message.
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long.clone());
    let (collector, live) = CollectorEmitter::new();
    let (harness, _provider, _env) =
        harness_with_options(script, RetryPolicy::default(), Some(Arc::new(collector))).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };

    // Wait until the frames are durable, then kill the run.
    let session = harness.session().clone();
    let mut committed = 0usize;
    for _ in 0..200 {
        let records = pending_frames(&session).await;
        committed = records.len();
        if committed >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        committed >= 3,
        "expected the start/text_start/text_delta frames to be durable before the crash, got {committed}"
    );
    // Nothing was persisted as history yet — this is the pre-fix failure mode.
    let branch = harness
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("branch readable");
    assert_eq!(
        branch
            .iter()
            .filter(|entry| matches!(entry, rpi_harness::session::types::Entry::Message(_)))
            .count(),
        1,
        "only the prompt is history at this point"
    );

    let live_id = live
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(_),
                message_id,
            } => message_id.clone(),
            _ => None,
        })
        .expect("published assistant has ID");
    let bindings = session.find_records(&RecordQuery::default()).await.unwrap();
    assert!(bindings.iter().any(|record| matches!(record,
        LaneRecord::AssistantFrame(frame) if frame.op == rpi_harness::session::types::AssistantFrameOp::CommitStream
            && frame.frame.as_ref().and_then(|value| value.get("entryId")).and_then(|value| value.as_str()) == Some(live_id.as_str())
    )));
    run.abort();
    let _ = run.await;

    // Recover: the committed prefix becomes an interrupted message instead of
    // vanishing.
    let salvaged = rpi_harness::frame_progress::salvage_run_frames(&session, "unknown-run")
        .await
        .expect("salvage runs");
    assert!(salvaged.is_empty(), "a different run id salvages nothing");

    // Read the run id that actually wrote the frames.
    let records = pending_frames(&session).await;
    let run_id = records
        .iter()
        .filter_map(|record| record.run_id().map(str::to_string))
        .next()
        .expect("frames carry a run id");

    let salvaged = rpi_harness::frame_progress::salvage_run_frames(&session, &run_id)
        .await
        .expect("salvage runs");
    assert_eq!(salvaged.len(), 1, "one stream was committed");
    let message = &salvaged[0];
    assert_eq!(
        message.stop_reason,
        rpi_ai::types::StopReason::Error,
        "a salvaged message is marked as an error, not a normal stop"
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some(rpi_harness::frame_progress::INTERRUPTED_NOTICE)
    );
    let salvaged_text = match message.content.first() {
        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
        other => panic!("expected salvaged text content, got {other:?}"),
    };
    assert!(
        !salvaged_text.is_empty() && long.starts_with(&salvaged_text),
        "the content streamed before the crash must survive as a prefix of the full reply; \
         salvaged {salvaged_text:?}"
    );
    assert!(
        salvaged_text.len() < long.len(),
        "the crash landed mid-stream, so the salvage must be partial"
    );
    assert!(
        message.usage == rpi_ai::types::Usage::zero(),
        "a salvaged partial must not bill usage a retry would bill again"
    );
}

/// A run that finishes normally retires its frames: they were progress, not
/// history, so a later recovery must find nothing to salvage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_run_leaves_no_frames_to_salvage() {
    let script = FauxScript::new().with_text("all done");
    let (harness, _provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("finish cleanly", vec![])
        .await
        .expect("prompt completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let records = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
            ..Default::default()
        })
        .await
        .expect("frame records readable");
    assert!(
        !records.is_empty(),
        "frames were recorded while the run was in flight"
    );

    let salvaged =
        rpi_harness::frame_progress::salvage_run_frames(harness.session(), &result.run_id)
            .await
            .expect("salvage runs");
    assert!(
        salvaged.is_empty(),
        "a committed run must not be salvaged: {salvaged:?}"
    );
}

/// Reopen an existing session in a fresh harness.
///
/// This is what a user does after a crash (`rpi` again on the same session), and
/// opening the harness is what runs `finish_interrupted_operation`.
async fn reopen_harness(session: rpi_harness::session::session::Session) -> AgentHarness {
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let model = provider.default_model().clone();
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: true,
    };
    AgentHarness::create(options)
        .await
        .expect("reopen harness on the crashed session")
}

/// The whole point of frame progress, end to end: crash mid-stream, reopen the
/// session, and get the committed prefix back as an interrupted message instead
/// of an empty run.
///
/// Exercises the real recovery entry point (`finish_interrupted_operation`, run
/// by `AgentHarness::create`) rather than calling salvage directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_a_crashed_session_restores_the_committed_prefix() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long.clone());
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };
    let session = harness.session().clone();
    for _ in 0..200 {
        let records = pending_frames(&session).await;
        if records.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;

    // The crash left the operation dangling: nothing closed it.
    let open = session
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");
    assert_eq!(
        open.len(),
        1,
        "a crashed run leaves exactly one open operation"
    );

    drop(harness);
    let recovered = reopen_harness(session).await;

    // Recovery closed the operation and preserved the output.
    let open = recovered
        .session()
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");
    assert!(
        open.is_empty(),
        "reopening the session must close the interrupted operation, still open: {open:?}"
    );

    let entries = recovered
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("branch readable");
    let messages: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 2, "prompt + recovered assistant message");

    let assistant = match &messages[1].message {
        AgentMessage::Assistant(assistant) => assistant.clone(),
        other => panic!("expected an assistant message, got {other:?}"),
    };
    assert_eq!(
        assistant.stop_reason,
        rpi_ai::types::StopReason::Error,
        "the recovered message is marked as interrupted"
    );
    assert_eq!(
        assistant.error_message.as_deref(),
        Some(rpi_harness::frame_progress::INTERRUPTED_NOTICE)
    );
    let text = match assistant.content.first() {
        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
        other => panic!("expected recovered text content, got {other:?}"),
    };
    assert!(
        !text.is_empty() && long.starts_with(&text) && text.len() < long.len(),
        "the recovered text must be the partial prefix committed before the crash, got {text:?}"
    );
}

/// Recovery must be idempotent: opening the same crashed session again must not
/// append a second copy of the salvaged message.
///
/// The early return in `finish_interrupted_operation` (no open operation → do
/// nothing) is what guarantees this, and nothing else in the recovery path
/// writes a `ClearRun`, so this is the invariant worth pinning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovering_the_same_crashed_session_twice_does_not_duplicate() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };
    let session = harness.session().clone();
    for _ in 0..200 {
        let records = pending_frames(&session).await;
        if records.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;
    drop(harness);

    let count_messages = |harness: &AgentHarness| {
        let session = harness.session().clone();
        async move {
            session
                .view("main")
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .expect("branch readable")
                .iter()
                .filter(|entry| matches!(entry, rpi_harness::session::types::Entry::Message(_)))
                .count()
        }
    };

    let first = reopen_harness(session).await;
    assert_eq!(
        count_messages(&first).await,
        2,
        "first recovery: prompt + interrupted message"
    );

    // Open the recovered session a second time.
    let second = reopen_harness(first.session().clone()).await;
    assert_eq!(
        count_messages(&second).await,
        2,
        "second recovery must not append the salvaged message again"
    );
}
/// A tool that never returns, so a run can be killed *during* tool execution —
/// the common crash case, and the one that produces an assistant message whose
/// tool calls have no results.
struct HangingTool {
    schema: rpi_ai::types::Tool,
}

impl HangingTool {
    fn new() -> Self {
        Self {
            schema: rpi_ai::types::Tool {
                name: "slow_tool".to_string(),
                description: "never returns".to_string(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::json!({ "type": "object", "properties": {} }),
                ),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for HangingTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "slow_tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        // Never resolves: the run is stuck inside tool execution.
        std::future::pending::<()>().await;
        unreachable!()
    }
}

/// Build a harness whose only tool hangs.
async fn harness_with_hanging_tool(script: FauxScript) -> (AgentHarness, Arc<FauxProvider>) {
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let tools: Vec<HarnessTool> = vec![
        HarnessTool::new(Arc::new(HangingTool::new())),
        HarnessTool::new(create_read_tool(&ctx, None)),
    ];
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    let metadata = SessionMetadata {
        id: "crash-tool".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: false,
    };
    let harness = AgentHarness::create(options).await.expect("create harness");
    (harness, provider)
}

/// Crashing *during* tool execution must not leave a transcript the next
/// request cannot be built from.
///
/// This is the failure mode of salvaging an assistant message verbatim: the
/// committed partial carries the tool call, its result never landed, and every
/// provider rejects an assistant message whose tool calls have no results
/// (Anthropic: "`tool_use` ids were found without `tool_result` blocks"). The
/// salvage therefore pairs each unresolved call with a synthetic error result,
/// which also tells the model and the user that the outcome is unknown rather
/// than "not applied".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashing_during_a_tool_call_leaves_a_valid_transcript() {
    let script = FauxScript::new().with_tool_call("slow_tool", serde_json::json!({}));
    let (harness, _provider) = harness_with_hanging_tool(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("run the slow tool", vec![]).await })
    };
    let session = harness.session().clone();

    // The assistant entry precedes the actual execution checkpoint. Wait for
    // the checkpoint itself: killing between those two writes is a different
    // (never entered execute) boundary, not this in-flight recovery scenario.
    let mut saw_tool_start = false;
    for _ in 0..300 {
        saw_tool_start = session
            .find_records(&RecordQuery {
                record_type: Some("tool_started"),
                ..Default::default()
            })
            .await
            .unwrap()
            .iter()
            .any(|record| matches!(record, LaneRecord::ToolStarted(started) if started.tool_name == "slow_tool"));
        if saw_tool_start {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        saw_tool_start,
        "the execution checkpoint must precede the crash"
    );

    // Kill the run inside tool execution: no tool result will ever exist.
    run.abort();
    let _ = run.await;

    // The write side closes the loop with the read side: the running tool is
    // recorded *before* it runs, so recovery can name the tool whose side effect
    // is unknown instead of reporting a generic "a tool might have run".
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("records readable");
    let started = records
        .iter()
        .find_map(|record| match record {
            LaneRecord::ToolStarted(started) if started.tool_name == "slow_tool" => Some(started),
            _ => None,
        })
        .expect("the running tool must be recorded before it runs");
    assert_eq!(
        started.effective_args,
        serde_json::json!({}),
        "the record must carry the arguments the tool was called with"
    );

    let report = rpi_harness::runtime::SessionRuntime::new(session.clone())
        .recover_lane("main")
        .await
        .expect("lane recovery runs");
    assert!(
        report
            .tool_frames
            .iter()
            .any(|frame| frame.tool_name == "slow_tool"),
        "recovery must track the in-flight tool: {:?}",
        report.tool_frames
    );
    // While the run is still open the recorded tool is ordinary in-flight work,
    // not yet a finding: only a run that *died* is worth reporting, and at this
    // point nothing has closed the operation. The finding (`DanglingToolFrame`)
    // appears once the operation is closed without its result landing, which is
    // covered by `runtime`'s `an_unlanded_tool_frame_names_the_tool`.
    assert!(
        !report.findings.iter().any(|finding| matches!(
            finding,
            rpi_harness::runtime::RecoveryFinding::DanglingToolFrame { .. }
        )),
        "an open run's tool is in flight, not dangling: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            rpi_harness::runtime::RecoveryFinding::OrphanedOperation { .. }
        )),
        "the crash left the operation open: {:?}",
        report.findings
    );
    assert!(
        !report.is_corrupt(),
        "an interrupted tool is not log corruption: {report:?}"
    );

    drop(harness);

    let recovered = reopen_harness(session).await;
    // Read the *branch path* specifically: the repaired sequence has to be
    // reachable from the leaf, because that is what the next request is built
    // from. Asserting on all entries would pass even if the results were
    // orphaned off the branch.
    let entries = recovered
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");

    let mut tool_call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for entry in &entries {
        if let rpi_harness::session::types::Entry::Message(message) = entry {
            match &message.message {
                AgentMessage::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let rpi_ai::types::Content::ToolCall(call) = block {
                            tool_call_ids.push(call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(result) => {
                    assert!(
                        result.is_error,
                        "the stand-in result must be marked as an error"
                    );
                    let text = match result.content.first() {
                        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
                        other => panic!("expected text in the stand-in result, got {other:?}"),
                    };
                    assert!(
                        text.contains("unknown"),
                        "the stand-in result must say the outcome is unknown, got {text:?}"
                    );
                    result_ids.push(result.tool_call_id.clone());
                }
                _ => {}
            }
        }
    }

    assert!(
        !tool_call_ids.is_empty(),
        "the recovered transcript must still carry the interrupted tool call"
    );
    for id in &tool_call_ids {
        assert!(
            result_ids.contains(id),
            "every recovered tool call needs a result or the next request is invalid; \
             {id} has none (calls={tool_call_ids:?}, results={result_ids:?})"
        );
    }
}

/// A real run's record log must be a legal product of the record protocol.
///
/// This is the end-to-end check on the write side: commit-on-settle commits an
/// assistant message carrying tool calls *before* its tools run, and records a
/// `step_attempt` naming that entry. The reducer then deep-checks the log —
/// `step_attempt` series/result consistency, `write_deferred` targets, and the
/// `tool_started` ↔ assistant-ordinal match. Writing records the validator would
/// reject is worse than writing none, because a corrupt log blocks recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_run_leaves_a_valid_record_log() {
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "out.txt", "content": "hello" }),
        )
        .with_text("done");
    let (harness, _provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("write a file", vec![])
        .await
        .expect("run completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let session = harness.session();
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("records readable");
    let entries = session
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("entries readable");
    let open_operations = session
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");

    // The settled assistant step must be recorded, and its `result_entry_id`
    // must name an entry that really landed with that message's content.
    let attempts: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::StepAttempt(attempt) => Some(attempt),
            _ => None,
        })
        .collect();
    assert!(
        !attempts.is_empty(),
        "a run that streamed an assistant message must record its step"
    );
    for attempt in &attempts {
        let landed = entries.iter().any(|entry| {
            entry.id() == attempt.result_entry_id
                && matches!(
                    entry,
                    rpi_harness::session::types::Entry::Message(message)
                        if message.message.is_assistant()
                )
        });
        assert!(
            landed,
            "step attempt {} names result entry {} which did not land as an assistant message",
            attempt.base.id, attempt.result_entry_id
        );
    }

    // And the log as a whole is legal.
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations,
        records,
        entries,
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a completed run must leave a legal record log");
}

/// A retried attempt must not be committed: the harness re-runs the whole loop,
/// so persisting the failed attempt would leave a dead assistant message in
/// history and put two assistant turns in a row in front of the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_attempt_is_not_committed() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("Recovered after retry."),
    ]);
    let (harness, provider, _env) = harness_with_options(
        script,
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            ..RetryPolicy::default()
        },
        None,
    )
    .await;

    let result = harness
        .prompt_text("do the thing", vec![])
        .await
        .expect("run completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the provider should have been retried once"
    );

    // Exactly one assistant message in history: the recovered one. The failed
    // attempt's message must not be present.
    let entries = harness
        .session()
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("entries readable");
    let assistants: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => match &message.message {
                AgentMessage::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "the failed attempt must not be committed, got {} assistant messages",
        assistants.len()
    );
    assert_eq!(assistants[0].stop_reason, rpi_ai::types::StopReason::Stop);
    assert!(
        assistants[0].error_message.is_none(),
        "the committed message must be the recovered one, not the failed attempt"
    );
}

/// A tool that hangs on its first invocation and answers on any later one.
///
/// This is what makes a replay observable: the first call is killed mid-flight
/// (leaving its outcome unknown), and a re-run returns a real result. The call
/// count proves which happened.
struct ReplayableTool {
    schema: rpi_ai::types::Tool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Which invocation hangs. `1` makes the tool a one-shot trap (crash on the
    /// first call); `2` lets a first call succeed so a *later* turn can be the one
    /// that dies, which is what a multi-turn crash looks like.
    hang_at: usize,
}

impl ReplayableTool {
    /// Shares `calls` with any other instance built from the same counter, so a
    /// reopened harness replays into the same counter the crashed one used.
    fn with_hang_at(calls: Arc<std::sync::atomic::AtomicUsize>, hang_at: usize) -> Self {
        Self {
            schema: rpi_ai::types::Tool {
                name: "flaky_read".to_string(),
                description: "hangs on a chosen call".to_string(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::json!({ "type": "object", "properties": {} }),
                ),
                constrained_sampling: None,
            },
            calls,
            hang_at,
        }
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for ReplayableTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "flaky_read"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        use std::sync::atomic::Ordering;
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.hang_at {
            // This invocation never returns: the run is killed inside it.
            std::future::pending::<()>().await;
        }
        Ok(rpi_agent::AgentToolResult::text("REAL RESULT FROM RE-RUN"))
    }
}

/// Build a harness whose meaningful tool is replayable-or-not per policy.
///
/// `calls` is threaded through so the *reopened* harness can share the counter
/// with the crashed one: that is how "was the tool re-run?" becomes observable,
/// and the shared counter makes a re-run answer instead of hanging again.
async fn replayable_harness(
    session: Session,
    replay: ToolReplay,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
) -> AgentHarness {
    replayable_harness_hanging_at(session, replay, calls, allow_existing_session, provider, 1).await
}

/// Same, but with an explicit retry policy.
async fn replayable_harness_with_retry(
    session: Session,
    replay: ToolReplay,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
    retry: RetryPolicy,
) -> AgentHarness {
    replayable_harness_full(
        session,
        replay,
        calls,
        allow_existing_session,
        provider,
        1,
        retry,
    )
    .await
}

/// Same, but the flaky tool hangs on invocation `hang_at` instead of the first.
async fn replayable_harness_hanging_at(
    session: Session,
    replay: ToolReplay,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
    hang_at: usize,
) -> AgentHarness {
    replayable_harness_full(
        session,
        replay,
        calls,
        allow_existing_session,
        provider,
        hang_at,
        RetryPolicy::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn replayable_harness_full(
    session: Session,
    replay: ToolReplay,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
    hang_at: usize,
    retry: RetryPolicy,
) -> AgentHarness {
    let model = provider.default_model().clone();
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let replayable = ReplayableTool::with_hang_at(calls, hang_at);
    let tools: Vec<HarnessTool> = vec![
        HarnessTool::new(Arc::new(replayable)).with_replay(replay),
        HarnessTool::new(create_read_tool(&ctx, None)),
    ];
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry,
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session,
    };
    AgentHarness::create(options).await.expect("create harness")
}

/// A fresh in-memory session for the replay tests.
fn replay_session() -> Session {
    let metadata = SessionMetadata {
        id: "replay-tool".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    Session::new(storage, None)
}

/// Crash a run inside a tool call, recover, and report the recovered tool
/// results plus how many times the tool was invoked in total.
async fn crash_inside_tool(replay: ToolReplay) -> (Vec<rpi_ai::types::ToolResultMessage>, usize) {
    crash_inside_tool_with(replay, replay).await
}

/// Same, but the tool may be declared differently at recovery time than it was
/// when the call was recorded — which is what the dual-confirmation rule is for.
async fn crash_inside_tool_with(
    recorded_replay: ToolReplay,
    recovery_replay: ToolReplay,
) -> (Vec<rpi_ai::types::ToolResultMessage>, usize) {
    let script = FauxScript::new().with_tool_call("flaky_read", serde_json::json!({}));
    let session = replay_session();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let harness = replayable_harness(
        session.clone(),
        recorded_replay,
        Arc::clone(&calls),
        false,
        FauxProvider::new(script),
    )
    .await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("read the flaky thing", vec![]).await })
    };
    let session = harness.session().clone();
    for _ in 0..300 {
        let records = session
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                ..Default::default()
            })
            .await
            .expect("records readable");
        let started = records.iter().any(|record| {
            matches!(record, LaneRecord::ToolStarted(frame) if frame.tool_name == "flaky_read")
        });
        if started {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;
    drop(harness);

    // Reopen with the tool registered: recovery can only replay a call whose
    // tool it knows about.
    let recovered = replayable_harness(
        session,
        recovery_replay,
        Arc::clone(&calls),
        true,
        FauxProvider::new(FauxScript::new().with_text("unused")),
    )
    .await;
    let entries = recovered
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    let results = entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => match &message.message {
                AgentMessage::ToolResult(result) => Some((**result).clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let calls = calls.load(std::sync::atomic::Ordering::SeqCst);
    (results, calls)
}

/// A tool declared `Safe` is re-run, so recovery recovers its **real** result
/// instead of downgrading the call to "outcome unknown".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayable_tool_is_re_run_and_its_real_result_recovered() {
    let (results, calls) = crash_inside_tool(ToolReplay::Safe).await;
    assert_eq!(
        calls, 2,
        "the tool must have been invoked twice: the killed call and the replay"
    );
    let result = results.first().expect("a tool result must be recorded");
    assert!(!result.is_error, "a successful replay is not an error");
    let joined = result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        joined.contains("REAL RESULT FROM RE-RUN"),
        "the replayed result must carry the tool's real output, got {joined:?}"
    );
    assert!(
        joined.contains("[recovered]"),
        "the result must say it came from a re-run, got {joined:?}"
    );
    assert!(
        joined.find("REAL RESULT").unwrap() < joined.find("[recovered]").unwrap(),
        "the tool's own output must come first, with the marker after it: {joined:?}"
    );
}

/// A tool declared `Never` must not be re-run: its side effect is unknown, and
/// re-running it could duplicate that effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreplayable_tool_is_not_re_run() {
    let (results, calls) = crash_inside_tool(ToolReplay::Never).await;
    assert_eq!(
        calls, 1,
        "an unreplayable tool must never be invoked a second time"
    );
    let result = results.first().expect("a tool result must be recorded");
    assert!(result.is_error, "the stand-in is marked as an error");
    let text = match result.content.first() {
        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("unknown"),
        "the stand-in must say the outcome is unknown, got {text:?}"
    );
    assert!(
        !text.contains("REAL RESULT"),
        "an unreplayable tool's result must not be fabricated, got {text:?}"
    );
}
/// A call recorded as safe must NOT be replayed when the tool no longer declares
/// itself safe.
///
/// Mirrors native's `call.replay === "safe" && tool?.replay === "safe"`. Believing
/// only the record would replay a call into a tool that has since been redefined
/// as unsafe — the exact case where a duplicate side effect would be invisible,
/// because the tool's own declaration is what says it can happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_safe_call_is_not_replayed_into_a_now_unsafe_tool() {
    let (results, calls) = crash_inside_tool_with(ToolReplay::Safe, ToolReplay::Never).await;
    assert_eq!(
        calls, 1,
        "the tool must not be re-run: its current declaration says Never"
    );
    let joined = results
        .first()
        .expect("a tool result must be recorded")
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        joined.contains("unknown"),
        "an unreplayed call must be recorded as unknown, got {joined:?}"
    );
}

/// Local helpers: an assistant message carrying one tool call, and its result.
fn assistant_with_one_tool_call(call_id: &str, name: &str) -> rpi_ai::types::AssistantMessage {
    let mut message =
        rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 0);
    message
        .content
        .push(rpi_ai::types::Content::ToolCall(rpi_ai::types::ToolCall {
            kind: rpi_ai::types::ToolCallType,
            id: call_id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        }));
    message
}

fn tool_result_of(call_id: &str, name: &str) -> rpi_ai::types::ToolResultMessage {
    rpi_ai::types::ToolResultMessage {
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: call_id.to_string(),
        tool_name: name.to_string(),
        content: Vec::new(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: 0,
    }
}

/// Crash a run inside a tool call, then reopen with `resume_script` available for
/// the run that continues afterwards.
async fn crashed_run_awaiting_resume(
    resume_script: FauxScript,
) -> (AgentHarness, Arc<std::sync::atomic::AtomicUsize>) {
    let script = FauxScript::new().with_tool_call("flaky_read", serde_json::json!({}));
    let session = replay_session();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let harness = replayable_harness(
        session.clone(),
        ToolReplay::Never,
        Arc::clone(&calls),
        false,
        FauxProvider::new(script),
    )
    .await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("read the flaky thing", vec![]).await })
    };
    for _ in 0..300 {
        let records = session
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                ..Default::default()
            })
            .await
            .expect("records readable");
        if records.iter().any(|record| {
            matches!(record, LaneRecord::ToolStarted(frame) if frame.tool_name == "flaky_read")
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;
    drop(harness);

    let reopened = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::clone(&calls),
        true,
        FauxProvider::new(resume_script),
    )
    .await;
    (reopened, calls)
}

/// Every assistant text block on the branch, joined.
async fn branch_assistant_text(harness: &AgentHarness) -> String {
    harness
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable")
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => match &message.message {
                AgentMessage::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reopening a session whose run died mid-tool must offer to continue it, and
/// continuing must produce the next assistant turn **without a new user prompt**.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_run_is_continued_without_a_new_prompt() {
    let (recovered, _calls) =
        crashed_run_awaiting_resume(FauxScript::new().with_text("continued after resume")).await;

    let pending = recovered
        .pending_resume()
        .expect("recovery repaired a mid-loop run, so there is something to continue");
    assert_eq!(pending.attempt, 0, "this is the first resume");

    let resumed = recovered
        .resume_pending()
        .await
        .expect("resume runs")
        .expect("a run was continued");
    assert!(
        matches!(resumed.outcome, HarnessRunOutcome::Completed { .. }),
        "the continued run must complete: {:?}",
        resumed.outcome
    );
    let joined = branch_assistant_text(&recovered).await;
    assert!(
        joined.contains("continued after resume"),
        "the continued run's output must be in the transcript, got {joined:?}"
    );
    // The resume announces itself, so a run starting without the user asking is
    // never a mystery.
    let notices: Vec<String> = recovered
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable")
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => {
                rpi_harness::messages::custom_data(&message.message).map(|data| data.custom_type)
            }
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|kind| kind == "runResume"),
        "the resume must record a `runResume` notice so a self-started run is explained, got {notices:?}"
    );

    assert!(
        recovered.pending_resume().is_none(),
        "a resumed run must not be offered again"
    );
}

/// A run that keeps dying must not be resumed forever: the attempt budget is
/// durable, because the crash that makes it matter destroys any in-memory
/// counter that could have held it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_keeps_dying_is_not_resumed_forever() {
    use rpi_harness::session::types::{
        OperationFinishedRecord, OperationIntent, OperationOutcome, OperationStartedRecord,
        RecordBase,
    };
    use std::collections::BTreeMap;

    let session = replay_session();
    // A branch that ends mid-loop: the interrupted run's tool result is the tip.
    session
        .view("main")
        .append_message(AgentMessage::Assistant(Box::new(
            assistant_with_one_tool_call("call-1", "flaky_read"),
        )))
        .await
        .expect("append assistant");
    session
        .view("main")
        .append_message(AgentMessage::ToolResult(Box::new(tool_result_of(
            "call-1",
            "flaky_read",
        ))))
        .await
        .expect("append tool result");

    let base = |id: &str| RecordBase {
        id: id.to_string(),
        seq: 0,
        lane: "main".to_string(),
        timestamp: 0,
    };
    let run_intent =
        |resume_data: Option<BTreeMap<String, serde_json::Value>>| OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data,
        };

    // The chain so far: the original run, then three resumes, each of which died
    // the same way. Storage allows only one open operation per lane, so every
    // earlier one is closed and only the newest is left open — that newest run is
    // the one recovery repairs and would otherwise continue a fourth time.
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("orig-run"),
            source_leaf_id: None,
            intent: run_intent(None),
        }))
        .await
        .expect("append original run");
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("orig-run-done"),
            run_id: "orig-run".to_string(),
            outcome: OperationOutcome::Aborted,
            error: None,
            result: None,
        }))
        .await
        .expect("close original run");

    for attempt in 1..=3u64 {
        let run_id = format!("resume-{attempt}");
        session
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: base(&run_id),
                source_leaf_id: None,
                intent: run_intent(Some(BTreeMap::from([
                    (
                        "resumedFrom".to_string(),
                        serde_json::Value::String("orig-run".to_string()),
                    ),
                    (
                        "resumeAttempt".to_string(),
                        serde_json::Value::from(attempt),
                    ),
                ]))),
            }))
            .await
            .expect("append resume run");
        // Only the last resume is left open; the earlier ones settled.
        if attempt < 3 {
            session
                .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    base: base(&format!("{run_id}-done")),
                    run_id: run_id.clone(),
                    outcome: OperationOutcome::Aborted,
                    error: None,
                    result: None,
                }))
                .await
                .expect("close resume run");
        }
    }

    let recovered = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        true,
        FauxProvider::new(FauxScript::new().with_text("unused")),
    )
    .await;

    assert!(
        recovered.pending_resume().is_none(),
        "a run whose resume budget is spent must not be offered again"
    );
    assert!(
        recovered
            .resume_pending()
            .await
            .expect("resume is a no-op")
            .is_none(),
        "nothing may be resumed once the budget is spent"
    );
}

/// Resume-decision matrix, part 1: a crash **while streaming the answer** must
/// NOT be auto-resumed.
///
/// The salvaged partial is an assistant message, so the branch tip is an
/// assistant turn. Continuing from it would hand the provider a trailing
/// assistant message — which Anthropic rejects outright (and `pi-agent`'s own
/// `run_agent_loop_continue` refuses for the same reason, see `agent_loop.rs`).
/// The run therefore ends with the salvaged partial and the user decides what is
/// next; that is also what upstream does, because an errored assistant response
/// finishes the turn rather than asking for another one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_mid_stream_is_not_auto_resumed() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("a long answer please", vec![]).await })
    };
    let session = harness.session().clone();
    // Wait for the durable precondition, not just a fixed number of polls:
    // under CI load the provider may not have produced any frame after 2s.
    // Aborting before a frame exists tests an empty run, not stream recovery.
    // Frames now live in the progress store, not the history record list.
    let started = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let records = pending_frames(&session).await;
            if !records.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    run.abort();
    let _ = run.await;
    started.expect("provider must persist an assistant frame before simulating a stream crash");
    drop(harness);

    let recovered = reopen_harness(session).await;

    // The partial was salvaged, so the tip is the interrupted assistant turn…
    let entries = recovered
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    let tip = entries.last().expect("the prompt is at least there");
    let rpi_harness::session::types::Entry::Message(tip) = tip else {
        panic!("expected a message at the tip, got {tip:?}");
    };
    let tip_assistant = match &tip.message {
        AgentMessage::Assistant(assistant) => (**assistant).clone(),
        other => panic!("expected the salvaged assistant message, got {other:?}"),
    };
    assert_eq!(
        tip_assistant.error_message.as_deref(),
        Some(rpi_harness::frame_progress::INTERRUPTED_NOTICE),
        "the tip must be the salvaged partial"
    );

    // …and that is precisely why there is nothing to resume.
    assert!(
        recovered.pending_resume().is_none(),
        "a trailing assistant turn cannot be continued: the provider would reject it"
    );
    assert!(recovered
        .resume_pending()
        .await
        .expect("resume is a no-op")
        .is_none());
}

/// Resume-decision matrix, part 2: an explicit user abort must NOT be resumed.
///
/// This holds by construction — a resume requires an *open* operation, and an
/// abort closes it — so the test is here to keep it that way. Getting this wrong
/// would mean the agent restarting work the user deliberately stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicitly_aborted_run_is_not_resumed() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("a long answer please", vec![]).await })
    };
    // Let the run actually start before aborting it.
    let mut started = false;
    for _ in 0..200 {
        let records = harness
            .session()
            .find_records(&RecordQuery {
                record_type: Some("operation_started"),
                ..Default::default()
            })
            .await
            .expect("records readable");
        if !records.is_empty() {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        started,
        "the run must have started before it can be aborted"
    );

    harness.lane("main").abort().await.expect("abort succeeds");
    let outcome = run.await.expect("the run task finished");
    assert!(
        outcome.is_ok(),
        "an aborted run settles normally: {outcome:?}"
    );

    let session = harness.session().clone();
    drop(harness);

    // The abort closed the operation, so recovery finds nothing to repair and
    // nothing to continue.
    let open = session
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");
    assert!(
        open.is_empty(),
        "a user abort must close the operation, got {open:?}"
    );
    let recovered = reopen_harness(session).await;
    assert!(
        recovered.pending_resume().is_none(),
        "work the user stopped must stay stopped"
    );
}

/// Resume-decision matrix, part 3: a run that completed normally is not resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_run_is_not_resumed() {
    let script = FauxScript::new().with_text("all done");
    let (harness, _provider, _env) = harness_with(script).await;
    let result = harness
        .prompt_text("finish cleanly", vec![])
        .await
        .expect("prompt completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert!(
        harness.pending_resume().is_none(),
        "a completed run has nothing to continue"
    );
}

/// Wait until the run has started streaming, i.e. until it is past the loop's
/// steering-drain point at the top of the run.
///
/// Steering is delivered at loop boundaries, so a message typed before that point
/// is consumed by the run itself — realistic, but not what these tests are about:
/// they are about what happens to a message that is still *queued* when the
/// process dies.
async fn wait_until_streaming(session: &rpi_harness::session::Session) {
    for _ in 0..300 {
        let records = pending_frames(session).await;
        if !records.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the run never started streaming");
}

/// A steer message typed while the agent is working must survive a crash.
///
/// The queue used to be purely process-local (`Arc<Mutex<MessageQueue>>` in the
/// harness), so anything the user typed while the agent worked was lost if the
/// process died — and the `queue_enqueued` / `queue_cancelled` records that exist
/// to prevent exactly that were never written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_message_survives_a_restart() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("start something long", vec![]).await })
    };

    // Only meaningful once the run is past its steering drain point.
    wait_until_streaming(harness.session()).await;
    // Wait for the run to be in flight, then type a steer message into it.
    let mut queued = None;
    for _ in 0..300 {
        if let Ok(result) = harness
            .lane("main")
            .steer(AgentMessage::User(UserMessage::new(
                UserContent::Text("also check the tests".into()),
                0,
            )))
            .await
        {
            queued = Some(result.entry_id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let queued_id = queued.expect("the steer must be accepted while the run is in flight");

    // The enqueue is durable immediately, before anything consumes it.
    let records = harness
        .session()
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            record_type: Some("queue_enqueued"),
            ..Default::default()
        })
        .await
        .expect("records readable");
    assert_eq!(
        records.len(),
        1,
        "the enqueue must be recorded when the message is queued, got {records:?}"
    );

    // Kill the process mid-run, before the queue could be drained.
    run.abort();
    let _ = run.await;
    let session = harness.session().clone();
    drop(harness);

    // Reopening restores the message as still-queued.
    let recovered = reopen_harness(session).await;
    let queued_now = recovered
        .lane("main")
        .queued_messages()
        .await
        .expect("queued messages readable");
    assert_eq!(
        queued_now
            .steering
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["also check the tests"],
        "a steer typed before the crash must come back queued"
    );

    // And it can still be cancelled by the id it was given.
    let cancellation = recovered
        .lane("main")
        .cancel_queued(&queued_id)
        .await
        .expect("cancel succeeds");
    assert_eq!(
        cancellation.outcome,
        rpi_harness::agent_harness::CancelQueuedOutcome::Cancelled,
        "the restored item must be cancellable under its original id"
    );
    let after_cancel = recovered.lane("main").cancel_queued(&queued_id).await;
    assert!(
        after_cancel.is_ok(),
        "cancelling an already-cancelled item is not an error"
    );
}

/// A cancelled queued message must NOT come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_queued_message_does_not_come_back() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("start something long", vec![]).await })
    };

    // Only meaningful once the run is past its steering drain point.
    wait_until_streaming(harness.session()).await;
    let mut queued_id = None;
    for _ in 0..300 {
        if let Ok(result) = harness
            .lane("main")
            .steer(AgentMessage::User(UserMessage::new(
                UserContent::Text("never mind".into()),
                0,
            )))
            .await
        {
            queued_id = Some(result.entry_id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let queued_id = queued_id.expect("the steer must be accepted");
    harness
        .lane("main")
        .cancel_queued(&queued_id)
        .await
        .expect("cancel succeeds");

    run.abort();
    let _ = run.await;
    let session = harness.session().clone();
    drop(harness);

    let recovered = reopen_harness(session).await;
    let queued_now = recovered
        .lane("main")
        .queued_messages()
        .await
        .expect("queued messages readable");
    assert!(
        queued_now.steering.is_empty(),
        "a cancelled message must not be restored, got {:?}",
        queued_now.steering
    );
}

/// The queue records must be a legal record log — the reducer matches a
/// cancellation against its enqueue on the run id, so a mismatch would surface
/// as corruption rather than as a lost message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_records_validate_against_the_reducer() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("start something long", vec![]).await })
    };

    // Only meaningful once the run is past its steering drain point.
    wait_until_streaming(harness.session()).await;
    let mut queued_id = None;
    for _ in 0..300 {
        if let Ok(result) = harness
            .lane("main")
            .steer(AgentMessage::User(UserMessage::new(
                UserContent::Text("queued then cancelled".into()),
                0,
            )))
            .await
        {
            queued_id = Some(result.entry_id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let queued_id = queued_id.expect("the steer must be accepted");
    harness
        .lane("main")
        .cancel_queued(&queued_id)
        .await
        .expect("cancel succeeds");

    run.abort();
    let _ = run.await;
    let session = harness.session().clone();
    drop(harness);

    // Reopening settles the interrupted operation, then the whole log must be
    // legal: the reducer matches a cancellation against its enqueue on the run
    // id, so a mismatch would show up here as corruption.
    let recovered = reopen_harness(session).await;
    let session = recovered.session();
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations: session
            .find_open_operations("main", None)
            .await
            .expect("open operations readable"),
        records: session
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("records readable"),
        entries: session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("entries readable"),
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("queue records must form a legal record log");
}

/// A crash *during* compaction must leave a session that is valid, correctly
/// reported, and still usable.
///
/// Compaction needs no resume machinery, and this test is what pins that claim.
/// The pre-run `should_compact` check re-evaluates on the next run, so a
/// compaction that never landed is simply decided again — native relies on the
/// same property ("a committed threshold compaction is its own durable marker —
/// any crash re-entry sees the newer compaction and skips").
///
/// What has to be true is that the half-finished attempt leaves nothing broken:
/// the `write_deferred` it recorded is reported (the entry it promised never
/// landed), the record log stays legal, and the lane still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_during_compaction_leaves_a_usable_session() {
    use rpi_harness::runtime::{RecoveryDecision, RecoveryFinding, SessionRuntime};
    use rpi_harness::session::types::{
        CompactionReason, OperationIntent, OperationStartedRecord, ProvisionedEntryJSON,
        RecordBase, StepAttemptRecord, StepKind, WriteDeferredRecord,
    };

    let session = replay_session();
    session
        .view("main")
        .append_message(AgentMessage::User(UserMessage::new(
            UserContent::Text("summarize this".into()),
            0,
        )))
        .await
        .expect("append a message");

    let base = |id: &str| RecordBase {
        id: id.to_string(),
        seq: 0,
        lane: "main".to_string(),
        timestamp: 0,
    };

    // The run that died mid-compaction.
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run-1"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .expect("append the run");

    // What `persist_compaction_entry` writes before committing: the intent to
    // commit entry `compaction-1`. Only `id` matters for this test — the entry
    // never landed, and the reducer's deep comparison only runs when it exists.
    let target: ProvisionedEntryJSON = serde_json::json!({
        "type": "compaction",
        "id": "compaction-1",
        "summary": "a summary that was never committed",
    });
    session
        .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
            base: base("wd-1"),
            run_id: "run-1".to_string(),
            target,
        }))
        .await
        .expect("append the deferred write");
    session
        .append_record(LaneRecord::StepAttempt(StepAttemptRecord {
            base: base("sa-1"),
            run_id: "run-1".to_string(),
            step: StepKind::Compaction,
            attempt: 1,
            result_entry_id: "compaction-1".to_string(),
            compaction_reason: Some(CompactionReason::Threshold),
        }))
        .await
        .expect("append the step attempt");

    // Reopen: recovery settles the dead run.
    let recovered = reopen_harness(session.clone()).await;
    assert!(
        recovered
            .session()
            .find_open_operations("main", None)
            .await
            .expect("open operations readable")
            .is_empty(),
        "recovery must settle the operation that died mid-compaction"
    );

    // The half-finished intent is reported rather than silently dropped: the
    // entry it promised never landed.
    let runtime = SessionRuntime::new(recovered.session().clone());
    let report = runtime.recover_lane("main").await.expect("recovery runs");
    assert!(
        report
            .findings
            .iter()
            .any(|finding| matches!(finding, RecoveryFinding::DanglingWrite { run_id } if run_id == "run-1")),
        "a deferred write whose entry never landed must be reported: {:?}",
        report.findings
    );
    assert!(
        runtime
            .reconcile(&report)
            .contains(&RecoveryDecision::DropDangling {
                run_id: "run-1".to_string()
            }),
        "and reconciled to a drop, not to corruption"
    );

    // The log as a whole is still legal: a compaction step whose result is absent
    // is exactly the deferred semantics the reducer allows.
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations: recovered
            .session()
            .find_open_operations("main", None)
            .await
            .expect("open operations readable"),
        records: recovered
            .session()
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("records readable"),
        entries: recovered
            .session()
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("entries readable"),
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a crash during compaction must leave a legal record log");

    // And the lane still works: a fresh run completes normally.
    let script = FauxScript::new().with_text("running fine");
    let harness = replayable_harness(
        recovered.session().clone(),
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        true,
        FauxProvider::new(script),
    )
    .await;
    let result = harness
        .prompt_text("carry on", vec![])
        .await
        .expect("a fresh run completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "the lane must be usable after a compaction crash: {:?}",
        result.outcome
    );
}

/// A multi-turn run records one `step_attempt` per settled assistant message, and
/// the numbers legitimately restart at 1 for each of them.
///
/// The reducer's "consecutive attempts" rule applies to a *live* series, i.e. one
/// whose previous result has not landed yet (`validate_attempt_sequence` treats a
/// previous result with an earlier sequence as ending the series). Two assistant
/// messages in one run are therefore two independent series, each starting at 1 —
/// `[1, 1]` is correct, not corruption. This test pins that, because it is the
/// opposite of what a reader would guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_turn_run_leaves_a_valid_record_log() {
    let script = FauxScript::new()
        .with_tool_call("read", serde_json::json!({ "path": "a.txt" }))
        .with_tool_call("read", serde_json::json!({ "path": "b.txt" }))
        .with_text("done");
    let (harness, _provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("read two files", vec![])
        .await
        .expect("run completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let session = harness.session();
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("records readable");
    let attempts: Vec<u32> = records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::StepAttempt(attempt) => Some(attempt.attempt),
            _ => None,
        })
        .collect();
    assert_eq!(
        attempts,
        vec![1, 1],
        "each settled assistant message starts its own attempt series"
    );

    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations: session
            .find_open_operations("main", None)
            .await
            .expect("open operations readable"),
        records,
        entries: session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("entries readable"),
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a multi-turn run must leave a legal record log");
}

/// A crash in the middle of a **multi-turn** run must be resumed with the whole
/// committed prefix intact and the rest of the work finished.
///
/// Every recovery test so far used a single-turn run. A long task is multi-turn —
/// assistant, tools, assistant, tools — so this is the realistic case, and it
/// exercises several things at once: one entry per settled assistant message, one
/// result per tool call, the salvage of only the *last* stream, and the resume
/// gate seeing a tool result at the tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_turn_run_resumes_after_a_crash_in_a_later_tool_call() {
    let script = FauxScript::new()
        .with_tool_call("flaky_read", serde_json::json!({}))
        .with_tool_call("flaky_read", serde_json::json!({}))
        .with_text("all done");
    let session = replay_session();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // The first call answers; the second hangs, so the run dies on turn 2.
    let harness = replayable_harness_hanging_at(
        session.clone(),
        ToolReplay::Never,
        Arc::clone(&calls),
        false,
        FauxProvider::new(script),
        2,
    )
    .await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("read it twice", vec![]).await })
    };
    // Wait until the second tool call is the one in flight.
    let mut in_second = false;
    for _ in 0..500 {
        if calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
            in_second = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(in_second, "the run must have reached its second tool call");
    run.abort();
    let _ = run.await;
    drop(harness);

    let recovered = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::clone(&calls),
        true,
        FauxProvider::new(FauxScript::new().with_text("carried on to the end")),
    )
    .await;

    // The committed prefix is intact: two assistant turns, two tool results (the
    // second recorded as unknown, since the tool never returned).
    let entries = recovered
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    let assistants = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                rpi_harness::session::types::Entry::Message(message)
                    if matches!(message.message, AgentMessage::Assistant(_))
            )
        })
        .count();
    let results = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                rpi_harness::session::types::Entry::Message(message)
                    if matches!(message.message, AgentMessage::ToolResult(_))
            )
        })
        .count();
    assert_eq!(assistants, 2, "both assistant turns must survive");
    assert_eq!(
        results, 2,
        "every recorded tool call needs a result, or the next request is invalid"
    );

    // And the run continues from there.
    let pending = recovered
        .pending_resume()
        .expect("the branch ends at a tool result, so the run can continue");
    assert_eq!(pending.attempt, 0);
    let resumed = recovered
        .resume_pending()
        .await
        .expect("resume runs")
        .expect("the run continues");
    assert!(matches!(
        resumed.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let joined = branch_assistant_text(&recovered).await;
    assert!(
        joined.contains("carried on to the end"),
        "the resumed run's output must be in the transcript, got {joined:?}"
    );

    // The log is still legal after all of that.
    let session = recovered.session();
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations: session
            .find_open_operations("main", None)
            .await
            .expect("open operations readable"),
        records: session
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("records readable"),
        entries: session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("entries readable"),
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a multi-turn crash and resume must leave a legal record log");
}

/// A crash during a **parallel tool batch** must still leave a result for every
/// call in it.
///
/// The batch is all-or-nothing in the transcript, and that is what makes the
/// recovery rule ("resolve the unresolved calls of the assistant at the tip")
/// sufficient: `execute_tool_calls_parallel` emits tool-result messages only
/// *after every call in the batch has settled*, in source order
/// (`pi-agent/src/agent_loop.rs`). So the fast call's result cannot land while a
/// sibling is still running — either the batch emitted results, or none did and
/// the assistant message is still the tip.
///
/// This test pins that reliance: if result emission ever became incremental, the
/// tip would be a tool result while the assistant holding the unfinished call sat
/// one entry back, and recovery would leave that call with no result — an invalid
/// request. It would fail here rather than in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partially_completed_tool_batch_is_repaired() {
    use rpi_ai::providers::faux::{faux_assistant_message, FauxBlock};

    // One assistant message carrying two calls, so they form a single batch.
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::Message(faux_assistant_message(
            vec![
                FauxBlock::tool_call("flaky_read", serde_json::json!({})),
                FauxBlock::tool_call("flaky_read", serde_json::json!({})),
            ],
            rpi_ai::types::StopReason::ToolUse,
        )),
        FauxStep::text("batch finished"),
    ]);

    let session = replay_session();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let harness = replayable_harness_hanging_at(
        session.clone(),
        ToolReplay::Never,
        Arc::clone(&calls),
        false,
        FauxProvider::new(script),
        2,
    )
    .await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("run both", vec![]).await })
    };
    let mut second_in_flight = false;
    for _ in 0..500 {
        if calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
            second_in_flight = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        second_in_flight,
        "the batch must have reached its second call"
    );
    run.abort();
    let _ = run.await;
    drop(harness);

    let recovered = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::clone(&calls),
        true,
        FauxProvider::new(FauxScript::new().with_text("continued after the batch")),
    )
    .await;

    let entries = recovered
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    let mut call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for entry in &entries {
        if let rpi_harness::session::types::Entry::Message(message) = entry {
            match &message.message {
                AgentMessage::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let rpi_ai::types::Content::ToolCall(call) = block {
                            call_ids.push(call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(result) => result_ids.push(result.tool_call_id.clone()),
                _ => {}
            }
        }
    }
    assert_eq!(call_ids.len(), 2, "the batch must survive as one message");
    for id in &call_ids {
        assert!(
            result_ids.contains(id),
            "every call in the batch needs a result; {id} has none \
             (calls={call_ids:?}, results={result_ids:?})"
        );
    }

    let session = recovered.session();
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations: session
            .find_open_operations("main", None)
            .await
            .expect("open operations readable"),
        records: session
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("records readable"),
        entries: session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("entries readable"),
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a repaired batch must leave a legal record log");
}

/// A crash during retry backoff must not silently end the run.
///
/// When an attempt fails with a retryable provider error the harness sleeps
/// before retrying, and nothing is persisted in that window (the failed attempt's
/// message is deliberately not committed). The run was *going* to retry and the
/// user asked for a result, so recovery must retry rather than leave an
/// "interrupted" message and make the user ask again.
///
/// Recognising that relies on the durable `retry_pending` record: the frames
/// cannot answer it, because a failed attempt leaves no terminal frame and so its
/// reduced message looks merely `pending`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_during_retry_backoff_still_retries() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("recovered after the restart"),
    ]);

    // A long base delay opens the window: the run sleeps here, which is where the
    // crash lands.
    let session = replay_session();
    let harness = replayable_harness_with_retry(
        session.clone(),
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        false,
        FauxProvider::new(script),
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 30_000,
            ..RetryPolicy::default()
        },
    )
    .await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("ask something", vec![]).await })
    };
    // Wait for the scheduled retry to be recorded: that is the marker recovery
    // reads, and it is written immediately before the backoff sleep.
    let mut scheduled = false;
    for _ in 0..300 {
        let records = session
            .find_records(&RecordQuery {
                record_type: Some("retry_pending"),
                ..Default::default()
            })
            .await
            .expect("records readable");
        if !records.is_empty() {
            scheduled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(scheduled, "the retry must be recorded before the backoff");
    run.abort();
    let _ = run.await;
    drop(harness);

    let recovered = replayable_harness_with_retry(
        session,
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        true,
        FauxProvider::new(FauxScript::new().with_text("recovered after the restart")),
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 10,
            ..RetryPolicy::default()
        },
    )
    .await;

    // The failed attempt is not dressed up as salvaged content, and the run is
    // offered for continuation.
    let joined = branch_assistant_text(&recovered).await;
    assert!(
        !joined.contains(rpi_harness::frame_progress::INTERRUPTED_NOTICE),
        "a retryable failure is not salvaged content, got {joined:?}"
    );
    let pending = recovered
        .pending_resume()
        .expect("the run was about to retry, so it must be continued");
    assert_eq!(pending.attempt, 0);

    let resumed = recovered
        .resume_pending()
        .await
        .expect("resume runs")
        .expect("the run continues");
    assert!(matches!(
        resumed.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let joined = branch_assistant_text(&recovered).await;
    assert!(
        joined.contains("recovered after the restart"),
        "the retry must produce the successful attempt, got {joined:?}"
    );
}

/// A retry that *succeeded* must not leave a pending resume behind.
///
/// The `retry_pending` record stays in the log after the retry succeeds, so the
/// only thing that may keep it from re-triggering is the reserved entry actually
/// landing. This test is what pins that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successful_retry_leaves_nothing_pending() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("recovered without a restart"),
    ]);
    let session = replay_session();
    let harness = replayable_harness_with_retry(
        session,
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        false,
        FauxProvider::new(script),
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
            ..RetryPolicy::default()
        },
    )
    .await;

    let result = harness
        .prompt_text("ask something", vec![])
        .await
        .expect("the retry recovers in-process");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert!(
        harness.pending_resume().is_none(),
        "a retry that succeeded in-process leaves nothing to continue"
    );
}

/// A steering message the model was already given must survive a crash.
///
/// Injected messages are the only user messages that appear *inside* the loop, and
/// the run-end pass is what used to persist them. So a crash after injection and
/// before the run ended lost the text the model had already been given — and the
/// queue item was cancelled at drain time, so it was gone for good. Committing it
/// at its own settle is what closes that.
///
/// The steering message is queued *before* the run starts, so it is drained at the
/// loop's first boundary and injected deterministically. Afterwards the run dies
/// inside its second tool call, i.e. long before the run-end pass could persist
/// anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_injected_steering_message_survives_a_crash() {
    // Two tool-call turns and then text: the second turn's call hangs, so the run
    // really is still working (not finished) when it is killed.
    let script = FauxScript::new()
        .with_tool_call("flaky_read", serde_json::json!({}))
        .with_tool_call("flaky_read", serde_json::json!({}))
        .with_text("done");
    let session = replay_session();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Hang on the *second* invocation: the first turn's call completes, the second
    // turn's never does, so the run has a long tail to die in.
    let harness = replayable_harness_hanging_at(
        session.clone(),
        ToolReplay::Never,
        Arc::clone(&calls),
        false,
        FauxProvider::new(script),
        2,
    )
    .await;

    harness
        .lane("main")
        .steer(AgentMessage::User(UserMessage::new(
            UserContent::Text("also do the other thing".into()),
            0,
        )))
        .await
        .expect("steer is accepted while idle");

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("start something", vec![]).await })
    };

    // Wait until the injected text is an entry. This must happen at its own
    // settle; the run has not ended (it is still working towards the hanging
    // tool), so the run-end pass cannot be the reason it is there.
    let has_steer = |entries: &[rpi_harness::session::types::Entry]| {
        entries.iter().any(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => match &message.message {
                AgentMessage::User(user) => matches!(
                    &user.content,
                    UserContent::Text(text) if text.contains("also do the other thing")
                ),
                _ => false,
            },
            _ => false,
        })
    };
    let mut injected = false;
    for _ in 0..500 {
        let entries = harness
            .session()
            .view("main")
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .expect("branch readable");
        if has_steer(&entries) {
            injected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        injected,
        "the injected steering message must be committed when it settles, not at run end"
    );

    // The run must actually be mid-flight when it dies: wait for the hanging
    // second call, which only its second tool-call turn reaches.
    let mut reached_hang = false;
    for _ in 0..500 {
        if calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
            reached_hang = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        reached_hang,
        "the run must reach its second tool call before it is killed"
    );
    run.abort();
    let _ = run.await;
    drop(harness);

    // The text is still there after recovery.
    let recovered = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::clone(&calls),
        true,
        FauxProvider::new(FauxScript::new().with_text("continued")),
    )
    .await;
    let entries = recovered
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    assert!(
        has_steer(&entries),
        "the steering text the model was given must survive the crash"
    );
}

/// A crash before the first provider call must still continue the run.
///
/// This is upstream's `starting` state: the operation opened and the prompt was
/// persisted, then the process died before anything was asked. The branch tip is
/// still the prompt, so the run has produced nothing and should simply continue —
/// the user does not need to retype what they already sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_before_the_first_request_still_continues() {
    check_crash_before_first_request(false).await;
}

/// Extension journals are branch data, not an answer to the user's prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_entries_do_not_hide_a_crashed_user_prompt() {
    check_crash_before_first_request(true).await;
}

async fn check_crash_before_first_request(with_custom_entries: bool) {
    use rpi_harness::session::types::{OperationIntent, OperationStartedRecord, RecordBase};

    let session = replay_session();
    // The prompt the run persisted, then died on.
    session
        .view("main")
        .append_message(AgentMessage::User(UserMessage::new(
            UserContent::Text("what I asked for".into()),
            0,
        )))
        .await
        .expect("append the prompt");

    // The run that died before calling the provider: opened, never finished.
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "run-1".to_string(),
                seq: 0,
                lane: "main".to_string(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .expect("append the run");

    if with_custom_entries {
        for _ in 0..2 {
            session
                .append_custom_entry("durable-state", Some(serde_json::json!({"running": true})))
                .await
                .expect("append AgentStart journal");
        }
    }

    let recovered = replayable_harness(
        session,
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        true,
        FauxProvider::new(FauxScript::new().with_text("answered on the restart")),
    )
    .await;

    let pending = recovered
        .pending_resume()
        .expect("a run that produced nothing should be continued");
    assert_eq!(pending.attempt, 0);

    if with_custom_entries {
        recovered
            .session()
            .append_custom_entry(
                "durable-state",
                Some(serde_json::json!({"interrupted": true})),
            )
            .await
            .expect("append SessionStart journal before resuming");
    }

    let resumed = recovered
        .resume_pending()
        .await
        .expect("resume runs")
        .expect("the run continues");
    assert!(matches!(
        resumed.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let joined = branch_assistant_text(&recovered).await;
    assert!(
        joined.contains("answered on the restart"),
        "the continued run must answer, got {joined:?}"
    );
    assert!(recovered.pending_resume().is_none());
    let entries = recovered
        .session()
        .view("main")
        .find_entries_on_branch(&EntryQuery::default(), &BranchBounds::default())
        .await
        .unwrap();
    let users = entries.iter().filter(|entry| {
        matches!(entry, rpi_harness::session::types::Entry::Message(message)
            if matches!(message.message, AgentMessage::User(_)))
    });
    assert_eq!(
        users.count(),
        1,
        "resume must not duplicate the user prompt"
    );
}

/// A deferred provider response suspends the run, and resuming it finishes the
/// work — including executing tool calls that arrived *from the poll*.
///
/// This is the shape upstream's `assistant.effect_pending` / `deferred.*` states
/// cover: the provider answers "not yet, poll this handle later" instead of a
/// message. The run must stay suspended (its operation open) rather than be
/// reported as finished, and on resume the polled assistant message's tool calls
/// have to run **before** the next provider request — a request carrying
/// unanswered tool calls is rejected by every provider. That is why resuming is
/// entered *from* the assistant message rather than by asking again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_response_suspends_and_then_resumes_through_its_tools() {
    use rpi_ai::providers::faux::{faux_assistant_message, DeferredPoll};
    use rpi_ai::types::{DeferredHandle, StopReason};

    // Turn 1 calls a tool; turn 2's answer arrives through a poll.
    let script = FauxScript::new()
        .with_tool_call("read", serde_json::json!({ "path": "a.txt" }))
        .with_text("done");
    let (harness, provider, _env) = harness_with(script).await;
    let (collector, live) = CollectorEmitter::new();
    harness
        .set_agent_emitter(Some(Arc::new(collector)))
        .await
        .unwrap();

    // What the poll returns: an assistant message with a tool call. Its tools were
    // never run (they came from outside the loop), which is the case this test
    // exists for.
    let mut polled = faux_assistant_message("", StopReason::ToolUse);
    polled.content = vec![rpi_ai::types::Content::ToolCall(rpi_ai::types::ToolCall {
        kind: rpi_ai::types::ToolCallType,
        id: "polled-call".to_string(),
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": "b.txt" }),
        thought_signature: None,
        namespace: None,
        recorded: None,
    })];
    provider.push_deferred_poll(DeferredPoll::Message(polled));

    // A handle the run can park on. The provider id/model must match the registered
    // provider so `resume_deferred` resolves its capability.
    let handle = DeferredHandle {
        provider: provider.id().to_string(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().to_string(),
        id: "deferred-1".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };

    // Suspend: record the parked assistant message carrying the handle. This is
    // what `find_deferred_handle` reads.
    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    harness
        .session()
        .view("main")
        .append_message(AgentMessage::Assistant(Box::new(parked)))
        .await
        .expect("park the deferred message");

    // The suspended operation stays open, which is what makes it resumable.
    let _ = StopReason::Deferred;

    let result = harness
        .resume(&handle.id)
        .await
        .expect("resume drives the deferred handle");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "the resumed run must finish, got {:?}",
        result.outcome
    );

    // The polled message's tool call really ran: a result for it is on the branch,
    // not just an "unknown" stand-in.
    let entries = harness
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");
    let polled_result = entries.iter().find_map(|entry| match entry {
        rpi_harness::session::types::Entry::Message(message) => match &message.message {
            AgentMessage::ToolResult(result) if result.tool_call_id == "polled-call" => {
                Some((**result).clone())
            }
            _ => None,
        },
        _ => None,
    });
    let polled_result = polled_result.expect("the polled tool call must have been executed");
    let live = live.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &live, false).await;
    let records = harness
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    let started = records
        .iter()
        .find_map(|record| match record {
            LaneRecord::ToolStarted(started) if started.tool_call_id == "polled-call" => {
                Some(started)
            }
            _ => None,
        })
        .expect("polled call has a durable ToolStarted");
    assert_eq!(started.tool_index, 0);
    assert!(entries
        .iter()
        .any(|entry| entry.base().id == started.assistant_entry_id));
    assert!(live.iter().any(|event| matches!(event,
        AgentEvent::MessageStart { message: AgentMessage::ToolResult(result), message_id: Some(id) }
            if result.tool_call_id == "polled-call" && id == &started.result_entry_id)));
    assert_eq!(started.effective_args, serde_json::json!({"path":"b.txt"}));
    assert!(entries.iter().any(|entry| matches!(entry,
        rpi_harness::session::types::Entry::Message(entry) if entry.base.id == started.result_entry_id && matches!(&entry.message,
            AgentMessage::ToolResult(result) if result.tool_call_id == "polled-call"))));
    assert!(
        !polled_result.is_error || !polled_result.content.is_empty(),
        "the tool must actually have run, got {polled_result:?}"
    );
}

/// A poll that hands back another handle must keep the run parked.
///
/// The provider is allowed to answer "still not ready" indefinitely, so resuming
/// must loop rather than treat the poll as a finished turn: the run stays
/// suspended, its operation stays open, and the *new* handle is what the next
/// resume polls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_still_deferred_poll_keeps_the_run_parked() {
    use rpi_ai::providers::faux::{faux_assistant_message, DeferredPoll};
    use rpi_ai::types::{DeferredHandle, StopReason};

    let script = FauxScript::new().with_text("unused");
    let (harness, provider, _env) = harness_with(script).await;

    let handle = DeferredHandle {
        provider: provider.id().to_string(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().to_string(),
        id: "deferred-1".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    let next_handle = DeferredHandle {
        id: "deferred-2".to_string(),
        ..handle.clone()
    };
    // First poll: still deferred, with a fresh handle.
    provider.push_deferred_poll(DeferredPoll::StillDeferred(next_handle.clone()));

    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    harness
        .session()
        .view("main")
        .append_message(AgentMessage::Assistant(Box::new(parked)))
        .await
        .expect("park the deferred message");

    let result = harness.resume(&handle.id).await.expect("resume polls once");
    match result.outcome {
        HarnessRunOutcome::Suspended { deferred, .. } => assert_eq!(
            deferred.id, next_handle.id,
            "a still-deferred poll must carry the new handle forward"
        ),
        other => panic!("a still-deferred poll must stay suspended, got {other:?}"),
    }
    // The new handle is discoverable, so the next resume polls it in turn.
    assert!(
        harness.drive(&next_handle.id).await.is_ok(),
        "the follow-up handle must be drivable"
    );
}

/// A provider with no long-poll continuation must report that, not pretend the
/// run finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resuming_through_a_provider_without_the_capability_is_an_error() {
    use rpi_ai::providers::faux::faux_assistant_message;
    use rpi_ai::types::{DeferredHandle, StopReason};

    let script = FauxScript::new().with_text("unused");
    let (harness, provider, _env) = harness_with(script).await;

    // A handle naming a provider that is not registered at all.
    let handle = DeferredHandle {
        provider: "some-other-provider".to_string(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().to_string(),
        id: "deferred-1".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    harness
        .session()
        .view("main")
        .append_message(AgentMessage::Assistant(Box::new(parked)))
        .await
        .expect("park the deferred message");

    let error = harness
        .resume(&handle.id)
        .await
        .expect_err("an unresolvable handle must be an error, not a fake finish");
    assert!(
        format!("{error:?}").contains("no provider"),
        "the error must say which provider is missing, got {error:?}"
    );
}

/// Assert identity by event order, never by matching message content. Failed retry
/// attempts may be deliberately absent from history, but each attempt still has
/// its own non-reused native reservation.
async fn assert_live_entry_ids(
    harness: &AgentHarness,
    events: &[AgentEvent],
    allow_discarded: bool,
) {
    let mut current: Option<String> = None;
    let mut starts = std::collections::BTreeSet::new();
    for event in events {
        match event {
            AgentEvent::MessageStart { message_id, .. } => {
                let id = message_id.clone().expect("session-backed start has ID");
                assert!(
                    starts.insert(id.clone()),
                    "each message owns its reservation"
                );
                current = Some(id);
            }
            AgentEvent::MessageUpdate { message_id, .. } => {
                assert_eq!(
                    message_id, &current,
                    "thinking/text/tool deltas keep start ID"
                );
            }
            AgentEvent::MessageEnd {
                message_id,
                message,
            } => {
                assert_eq!(message_id, &current, "end keeps start ID");
                let id = message_id.as_deref().unwrap();
                match harness.get_entry(id).await.unwrap() {
                    Some(rpi_harness::session::types::Entry::Message(entry)) => {
                        // JSON Option<Value> round-trips Some(Null) as None.
                        // Compare the native durable representation, not that
                        // in-memory-only distinction in tool-result details.
                        let canonical = |value: &AgentMessage| {
                            serde_json::from_value::<AgentMessage>(
                                serde_json::to_value(value).unwrap(),
                            )
                            .unwrap()
                        };
                        assert_eq!(
                            canonical(&entry.message),
                            canonical(message),
                            "live end is the same durable history entry"
                        );
                    }
                    None if allow_discarded
                        && matches!(message, AgentMessage::Assistant(a) if a.stop_reason == rpi_ai::StopReason::Error) =>
                        {}
                    other => panic!("missing or wrong entry for {id}: {other:?}"),
                }
                current = None;
            }
            _ => {}
        }
    }
    assert!(current.is_none(), "all live messages ended");
    let records = harness
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    for record in records {
        if let LaneRecord::ToolStarted(tool) = record {
            assert!(
                starts.contains(&tool.result_entry_id),
                "tool result reuses ToolStarted ID"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_live_ids_match_history_for_text_thinking_tools_and_final_persist() {
    use rpi_ai::providers::faux::FauxBlock;
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(faux_assistant_message(
            vec![
                FauxBlock::thinking("private reasoning"),
                FauxBlock::text("writing"),
                FauxBlock::tool_call(
                    "write",
                    serde_json::json!({"path":"identity.txt","content":"same"}),
                ),
            ],
            rpi_ai::StopReason::ToolUse,
        )),
        FauxStep::text("same"),
    ]);
    let (collector, events) = CollectorEmitter::new();
    let (harness, _, _) =
        harness_with_options(script, RetryPolicy::default(), Some(Arc::new(collector))).await;
    harness.prompt_text("same", vec![]).await.unwrap();
    let events = events.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &events, false).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::MessageUpdate {
            assistant_message_event: rpi_ai::AssistantMessageEvent::ThinkingDelta { .. },
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_retry_attempt_ids_are_distinct_and_only_success_is_committed() {
    let mut failed = faux_assistant_message("same", rpi_ai::StopReason::Error);
    failed.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(failed.clone()),
        FauxStep::message(failed),
        FauxStep::text("same"),
    ]);
    let (collector, events) = CollectorEmitter::new();
    let retry = RetryPolicy {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 0,
        max_agent_delay_ms: 0,
    };
    let (harness, _, _) = harness_with_options(script, retry, Some(Arc::new(collector))).await;
    harness.prompt_text("same", vec![]).await.unwrap();
    let events = events.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &events, true).await;
    let ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(_),
                message_id,
            } => message_id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(harness.get_entry(&ids[0]).await.unwrap().is_none());
    assert!(harness.get_entry(&ids[1]).await.unwrap().is_none());
    assert!(harness.get_entry(&ids[2]).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancelled_assistant_keeps_its_live_entry_id() {
    let script = FauxScript::new()
        .with_tokens_per_second(50.0)
        .with_abort_after(std::time::Duration::from_millis(50))
        .with_text("cancelled response ".repeat(100));
    let (collector, events) = CollectorEmitter::new();
    let (harness, _, _) =
        harness_with_options(script, RetryPolicy::default(), Some(Arc::new(collector))).await;
    let result = harness.prompt_text("cancel", vec![]).await.unwrap();
    assert!(matches!(result.outcome, HarnessRunOutcome::Aborted { .. }));
    let events = events.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &events, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_explicit_delivery_ids_do_not_match_identical_contents() {
    use rpi_harness::frame_progress::FrameRecordingEmitter;
    use rpi_harness::settle::{SettleState, SettlingEmitter};
    let (harness, _, _) = harness_with(FauxScript::new()).await;
    let (collector, events) = CollectorEmitter::new();
    let recorder = Arc::new(FrameRecordingEmitter::new(
        Arc::new(collector),
        harness.session().clone(),
        "main",
        "identity-delivery",
    ));
    let state = Arc::new(SettleState::new(
        "identity-delivery",
        false,
        0,
        Default::default(),
    ));
    let emitter = SettlingEmitter::new(
        recorder.clone(),
        recorder,
        state.clone(),
        harness.clone(),
        Default::default(),
    );
    let message: AgentMessage =
        faux_assistant_message("identical", rpi_ai::StopReason::Stop).into();
    // A generated assistant with identical bytes must NOT consume queued IDs.
    state.push_injected("queued-1", message.clone());
    state.push_injected("queued-2", message.clone());
    emitter
        .emit(AgentEvent::MessageStart {
            message: message.clone(),
            message_id: None,
        })
        .await;
    emitter
        .emit(AgentEvent::MessageEnd {
            message: message.clone(),
            message_id: None,
        })
        .await;
    for _ in 0..2 {
        emitter
            .emit_injected(AgentEvent::MessageStart {
                message: message.clone(),
                message_id: None,
            })
            .await;
        emitter
            .emit(AgentEvent::MessageEnd {
                message: message.clone(),
                message_id: None,
            })
            .await;
    }
    let events = events.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &events, false).await;
    let ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart { message_id, .. } => message_id.clone(),
            _ => None,
        })
        .collect();
    assert_ne!(ids[0], "queued-1");
    assert_eq!(&ids[1..], &["queued-1", "queued-2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_missing_delivery_reservation_cancels_without_publishing_a_new_id() {
    use rpi_harness::frame_progress::FrameRecordingEmitter;
    use rpi_harness::settle::{SettleState, SettlingEmitter};
    let (harness, _, _) = harness_with(FauxScript::new()).await;
    let (collector, events) = CollectorEmitter::new();
    let recorder = Arc::new(FrameRecordingEmitter::new(
        Arc::new(collector),
        harness.session().clone(),
        "main",
        "identity-failure",
    ));
    let state = Arc::new(SettleState::new(
        "identity-failure",
        false,
        0,
        Default::default(),
    ));
    let signal = tokio_util::sync::CancellationToken::new();
    let emitter = SettlingEmitter::new(recorder.clone(), recorder, state, harness, signal.clone());
    let message: AgentMessage = UserMessage::new("missing", 0).into();
    emitter
        .emit_injected(AgentEvent::MessageStart {
            message: message.clone(),
            message_id: None,
        })
        .await;
    emitter
        .emit(AgentEvent::MessageStart {
            message,
            message_id: None,
        })
        .await;
    assert!(signal.is_cancelled());
    assert!(events.lock().unwrap().is_empty());
}

mod native_identity_bind_failure {
    use super::*;
    use rpi_harness::error::{SessionError, SessionResult};
    use rpi_harness::session::types::*;

    struct RefuseBindings {
        inner: Arc<dyn SessionStorage>,
    }

    #[async_trait::async_trait]
    impl SessionStorage for RefuseBindings {
        fn metadata(&self) -> SessionMetadata {
            self.inner.metadata()
        }
        async fn append_progress(
            &self,
            records: Vec<AssistantFrameRecord>,
        ) -> SessionResult<Vec<AssistantFrameRecord>> {
            self.inner.append_progress(records).await
        }
        async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
            self.inner.read_progress(run_id).await
        }
        async fn prune_completed_progress(&self) -> SessionResult<usize> {
            self.inner.prune_completed_progress().await
        }
        async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage> {
            self.inner.record_page(query).await
        }
        async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
            self.inner.get_lanes().await
        }
        async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
            self.inner.create_lane(lane, at).await
        }
        async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
            self.inner.move_lane(lane, to).await
        }
        async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
            self.inner.append_entry(entry, lane).await
        }
        async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
            if matches!(&record, LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::CommitStream)
            {
                return Err(SessionError::storage("controlled identity binding failure"));
            }
            self.inner.append_record(record).await
        }
        async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
            self.inner.get_entry(id).await
        }
        async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
            self.inner.find_entries(query).await
        }
        async fn find_entries_on_branch(
            &self,
            query: &EntryQuery,
            bounds: &BranchBounds,
            start: &str,
        ) -> SessionResult<Vec<Entry>> {
            self.inner
                .find_entries_on_branch(query, bounds, start)
                .await
        }
        async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
            self.inner.find_records(query).await
        }
        async fn find_open_operations(
            &self,
            lane: &str,
            limit: Option<usize>,
        ) -> SessionResult<Vec<OperationStartedRecord>> {
            self.inner.find_open_operations(lane, limit).await
        }
        async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
            self.inner.get_log(options).await
        }
        async fn get_name(&self) -> SessionResult<Option<String>> {
            self.inner.get_name().await
        }
        async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
            self.inner.set_name(name).await
        }
        async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
            self.inner.get_label(id).await
        }
        async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
            self.inner.set_label(id, label).await
        }
        async fn get_stats(&self) -> SessionResult<SessionStats> {
            self.inner.get_stats().await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_failed_binding_is_propagated_before_first_live_message() {
        let (collector, events) = CollectorEmitter::new();
        let (harness, _, _) = harness_with_options(
            FauxScript::new().with_text("never publish an unbound ID"),
            RetryPolicy::default(),
            Some(Arc::new(collector)),
        )
        .await;
        let storage = Arc::new(RefuseBindings {
            inner: harness.session().storage(),
        });
        harness
            .set_session(Session::new(storage, None))
            .await
            .unwrap();
        let error = harness
            .prompt_text("bind", vec![])
            .await
            .expect_err("binding failure must fail the run");
        assert!(error
            .to_string()
            .contains("controlled identity binding failure"));
        assert!(
            !events.lock().unwrap().iter().any(|event| matches!(
                event,
                AgentEvent::MessageStart { .. }
                    | AgentEvent::MessageUpdate { .. }
                    | AgentEvent::MessageEnd { .. }
            )),
            "no identity is published after a failed durable binding"
        );
    }
}

/// `abort_reclaiming(true)` must be atomic against a run starting concurrently.
///
/// The idle path is the one that used to leak: `abort()` returned
/// `NoActiveRun` from inside the gate, the caller then did `clear_queue()`
/// separately, and a run starting in between could drain the queue first — the
/// reply said "nothing queued" for messages that were actually consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_reclaiming_is_atomic_on_an_idle_lane() {
    let script = FauxScript::new()
        .with_text("slow answer")
        .with_tokens_per_second(2.0);
    let (harness, _provider, _env) = harness_with(script).await;

    // Queue something on an idle lane.
    harness
        .steer(AgentMessage::User(UserMessage::new(
            UserContent::Text("staged while idle".into()),
            0,
        )))
        .await
        .expect("steering is accepted on an idle lane");

    // Idle abort still hands the queue back (idempotent stop button)…
    let result = harness
        .abort_reclaiming(true)
        .await
        .expect("idle abort with idle_is_ok must succeed");
    assert!(
        result.run_id.is_empty(),
        "no run was active, so there is no run id: {:?}",
        result.run_id
    );
    assert_eq!(
        result.steer.len(),
        1,
        "the staged message must come back: {:?}",
        result.steer
    );
    // …and it is a transfer, not a copy.
    assert!(
        harness.queued_messages().await.unwrap().is_empty(),
        "the queue must be empty after the reclaim"
    );

    // The strict contract is unchanged for callers that only want to cancel.
    let strict = harness.abort().await;
    assert!(
        strict.is_err(),
        "`abort()` must keep reporting NoActiveRun on an idle lane"
    );
}

/// `turn_end.pending_messages` must report the queue depth **without draining**.
///
/// The two drain hooks (`get_steering_messages` / `get_follow_up_messages`)
/// consume the queue, so they cannot answer "how many are left?" — asking would
/// eat the messages. The harness therefore installs a separate read-only
/// `pending_message_count` hook wired to the same queues.
///
/// This test pins both halves: the count is right, and asking does not disturb
/// the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_end_reports_pending_count_without_draining() {
    // One response per turn: the initial prompt plus each queued message
    // injects another turn.
    let script = FauxScript::new().with_text("done");
    script.append_responses(vec![
        FauxStep::text("done"),
        FauxStep::text("done"),
        FauxStep::text("done"),
        FauxStep::text("done"),
    ]);
    let (harness, _provider, _env) = harness_with(script).await;

    // Park two messages in the steering queue and one in follow-up.
    let msg =
        |text: &str| AgentMessage::User(UserMessage::new(UserContent::Text(text.to_string()), 0));
    for text in ["a", "b"] {
        harness.steer(msg(text)).await.unwrap();
    }
    harness.follow_up(msg("c")).await.unwrap();

    let queued = harness.queued_messages().await.unwrap();
    assert_eq!(
        queued.steering.len() + queued.follow_up.len(),
        3,
        "the three messages must actually be queued: {queued:?}"
    );

    let result = harness
        .prompt_text("go", vec![])
        .await
        .expect("prompt completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );

    // Whatever the loop consumed, nothing may be left behind: every queued
    // message either ran (drained) or is still accounted for.
    let after = harness.queued_messages().await.unwrap();
    assert!(
        after.steering.is_empty() && after.follow_up.is_empty(),
        "the run must have consumed the queue, got {after:?}"
    );
}

/// `StopRound` ends the run at the turn boundary **without** truncating the
/// answer, and is one-shot.
///
/// This is the gentle counterpart of abort: abort interrupts the in-flight
/// provider call, so the answer the user is reading gets cut off. "Stop after
/// this round" must instead let the current turn finish and only prevent the
/// loop from starting another one.
///
/// The request is issued while the run is **active**, which is the only case
/// where it has an effect (see `stop_round_while_idle_is_a_no_op`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_round_finishes_the_turn_then_stops() {
    // First turn: some text **then** a tool call, streamed slowly. The text
    // gives the test a window to issue the request while the turn is still
    // running; without it the turn can complete before `request_stop_round()`.
    let first = FauxStep::Message(faux_assistant_message(
        vec![
            FauxBlock::text("working on it"),
            FauxBlock::tool_call("bash", serde_json::json!({ "command": "true" })),
        ],
        rpi_ai::types::StopReason::ToolUse,
    ));
    // 4 tokens/s over ~6 tokens ≈ 1.5s: long enough to inject the request,
    // short enough to keep the suite fast.
    let script = FauxScript::new().with_tokens_per_second(4.0);
    script.set_responses(vec![first, FauxStep::text("second turn")]);
    let (harness, provider, _env) = harness_with(script).await;

    let prompt = tokio::spawn({
        let harness = harness.clone();
        async move { harness.prompt_text("go", vec![]).await }
    });

    // 4 tokens/s ⇒ the first chunk lands well after this sleep, so the request
    // is set while the turn is still in flight.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    harness.request_stop_round();

    let result = prompt.await.unwrap().expect("the prompt still settles");

    // The run completed rather than aborting: the first turn's answer is intact.
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "StopRound must complete the run, not abort it: {:?}",
        result.outcome
    );
    // And the second turn never happened — only the first provider call ran.
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the loop must not start another turn after StopRound"
    );
}

/// A `StopRound` requested while a retry is being scheduled must not be lost.
///
/// The request arrives on the boundary the scheduler just announced: without
/// consuming it before the next attempt, the retry runs anyway and the one-shot
/// request is honored a whole turn late.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_round_requested_during_retry_backoff_is_honored() {
    use rpi_ai::providers::faux::{faux_assistant_message, FauxBlock, FauxScript, FauxStep};

    struct StopOnRetryEmitter {
        harness: std::sync::Mutex<Option<AgentHarness>>,
    }
    impl AgentEmitter for StopOnRetryEmitter {
        fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
            self.try_emit(event);
            Box::pin(async {})
        }
        fn try_emit(&self, event: AgentEvent) {
            if matches!(event, AgentEvent::RetryScheduled { .. }) {
                if let Some(harness) = self.harness.lock().unwrap().as_ref() {
                    harness.request_stop_round();
                }
            }
        }
    }

    // First attempt fails retryably; a second response is available so a missed
    // StopRound shows up as an extra provider call.
    let failing = {
        let mut message =
            faux_assistant_message(Vec::<FauxBlock>::new(), rpi_ai::types::StopReason::Error);
        message.error_message = Some("overloaded_error: retryable".to_string());
        message
    };
    let script = FauxScript::new();
    script.append_responses(vec![
        FauxStep::Message(failing),
        FauxStep::text("second attempt"),
    ]);

    let emitter = Arc::new(StopOnRetryEmitter {
        harness: std::sync::Mutex::new(None),
    });
    let retry = rpi_harness::types::RetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 50,
        max_agent_delay_ms: 200,
    };
    let (harness, provider, _env) = harness_with_options(
        script,
        retry,
        Some(Arc::clone(&emitter) as Arc<dyn AgentEmitter>),
    )
    .await;
    *emitter.harness.lock().unwrap() = Some(harness.clone());

    let _ = harness.prompt_text("one", vec![]).await;
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a StopRound requested while the retry was scheduled must stop before the next attempt"
    );
}

/// A `StopRound` issued the instant `RunStart` is published must be honored.
///
/// This is the remote-client shape: a UI reacts to `RunStart` by asking the run
/// to stop after its first round. The idle-request reset therefore has to run
/// *before* the event is published — resetting afterwards erases exactly the
/// request the run just announced it was ready for, and the run takes an extra
/// provider call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_round_requested_at_run_start_is_honored() {
    // A tool call needs a second turn, so a lost StopRound shows up as two
    // provider calls instead of one.
    let script = FauxScript::new().with_tool_call("bash", serde_json::json!({ "command": "true" }));
    script.append_responses(vec![FauxStep::text("second turn")]);
    let (harness, provider, _env) = harness_with(script).await;

    // React to `RunStart` on the harness bus, which fires *during*
    // `prompt_text`, exactly where a remote UI would.
    let reactor = harness.clone();
    let mut watch = harness.events().watch(|| ());
    watch.start(Arc::new(move |event: &HarnessEvent| {
        if matches!(event, HarnessEvent::RunStart(_)) {
            reactor.request_stop_round();
        }
    }));
    std::mem::forget(watch);

    let _ = harness.prompt_text("one", vec![]).await;
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a StopRound issued when RunStart was published must stop before another turn"
    );
}

/// `StopRound` on an **idle** lane is a no-op: it must not make the next run
/// stop after its first turn.
///
/// The request only means something while a run is in flight ("finish *this*
/// round"). A leftover flag would silently turn the next prompt into a
/// one-turn run, which is exactly what the previous implementation did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_round_while_idle_is_a_no_op() {
    // Two turns would be needed for the tool call to continue, so a leaked flag
    // would be visible as "the run stopped after turn one".
    let script = FauxScript::new().with_tool_call("bash", serde_json::json!({ "command": "true" }));
    script.append_responses(vec![FauxStep::text("second turn")]);
    let (harness, provider, _env) = harness_with(script).await;

    // No run is active.
    harness.request_stop_round();

    let result = harness
        .prompt_text("one", vec![])
        .await
        .expect("the prompt settles");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "an idle StopRound must not stop the next run after one turn"
    );
}
