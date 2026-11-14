//! Offline in-process FFI provider -> actual agent loop. No shell is executed.
use super::*;
use crate::registry::ExtensionRegistry;
use rpi_agent::{
    AgentContext, AgentError, AgentTool, AgentToolResult, ToolExecutionMode, ToolResultPartial,
};
use rpi_ai::types::{Content, DoneReason, StopReason, Tool};
use std::sync::atomic::AtomicUsize;
use tokio_util::sync::CancellationToken;
#[path = "../../../rpi-agent/tests/common/mod.rs"]
mod common;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Ordinary,
    EndOnly,
    Both,
    NullEndOnly,
}
struct Fixture {
    mode: Mode,
    requests: AtomicUsize,
}

fn recording() -> serde_json::Value {
    serde_json::json!({"version":1,"isError":false,"result":{"content":[{"type":"text","text":"upstream-only"}],"details":{}}})
}
extern "C" fn unused_request(_: StbStringRef, _: *mut StbString, _: *mut c_void) -> i32 {
    1
}
extern "C" fn stream_request(
    _: StbStringRef,
    emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
    relay: *mut c_void,
    _: *const rpi_plugin_sdk::ProviderStreamControl,
    _: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    let fixture = unsafe { &*(user_data as *const Fixture) };
    let mut message = AssistantMessage::empty(rpi_ai::types::Api::Faux, "review183", "m", 0);
    if fixture.requests.fetch_add(1, Ordering::SeqCst) > 0 {
        message.stop_reason = StopReason::Stop;
        message.content.push(Content::text("done"));
    } else {
        let call = serde_json::json!({"type":"toolCall","id":"c1","name":"counter","arguments":{"command":"counter-only"}});
        let mut end = serde_json::json!({"type":"toolcall_end","contentIndex":0,"id":"c1","name":"counter","arguments":{"command":"counter-only"}});
        match fixture.mode {
            Mode::EndOnly | Mode::Both => end["recorded"] = recording(),
            Mode::NullEndOnly => end["recorded"] = serde_json::Value::Null,
            Mode::Ordinary => {}
        }
        if let Some(emit) = emit {
            for chunk in [
                serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"counter"}),
                end,
            ] {
                let rc = emit(StbString::from_string(chunk.to_string()), relay);
                if rc != 0 {
                    return rc;
                }
            }
        }
        let mut call = call;
        if matches!(fixture.mode, Mode::Both) {
            call["recorded"] = recording();
        }
        message.stop_reason = StopReason::ToolUse;
        message.content.push(serde_json::from_value(call).unwrap());
    }
    unsafe {
        *out = StbString::from_string(serde_json::to_string(&message).unwrap());
    }
    0
}

struct Counter {
    schema: Tool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AgentTool for Counter {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Counter"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("local-only"))
    }
}

async fn observe(mode: Mode, execution: ToolExecutionMode) -> (usize, bool, bool, String) {
    let fixture = Arc::new(Fixture {
        mode,
        requests: AtomicUsize::new(0),
    });
    let record = RegisteredProvider {
        provider_id: "review183".into(),
        base_url: "https://example.invalid".into(),
        api_style: "faux".into(),
        request_fn: unused_request,
        stream_fn: Some(stream_request),
        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
        user_data: Arc::as_ptr(&fixture) as *mut c_void,
    };
    let mut registry = ExtensionRegistry::new();
    registry.register_streaming_provider(record.clone());
    let provider = PluggableProvider::new(
        record,
        Arc::new(registry.snapshot()),
        PluginKeepalive::empty(),
        Handle::current(),
    );
    let streamed_marker = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let final_marker = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_seen = streamed_marker.clone();
    let final_seen = final_marker.clone();
    let stream_fn = rpi_agent::stream_fn(move |model, ctx, opts| {
        let (model, ctx, opts) = (model.clone(), ctx.clone(), opts.clone());
        let provider = provider.clone();
        let stream_seen = stream_seen.clone();
        let final_seen = final_seen.clone();
        let (mut prod, result) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            let mut upstream = provider.stream_simple(&model, &ctx, &opts).await;
            while let Some(event) = upstream.next().await {
                match &event {
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                        stream_seen.store(
                            serde_json::to_value(tool_call)
                                .unwrap()
                                .get("recorded")
                                .is_some(),
                            Ordering::SeqCst,
                        );
                    }
                    AssistantMessageEvent::Done {
                        reason: DoneReason::ToolUse,
                        message,
                    } => {
                        final_seen.store(
                            serde_json::to_value(&message.content[0])
                                .unwrap()
                                .get("recorded")
                                .is_some(),
                            Ordering::SeqCst,
                        );
                    }
                    _ => {}
                }
                prod.push(event).await;
            }
        });
        result
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Counter {
        calls: calls.clone(),
        schema: Tool {
            name: "counter".into(),
            description: "Counter only; no command execution".into(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
            ),
            constrained_sampling: None,
        },
    };
    let mut config = common::base_config();
    config.model = Model::new(
        "m",
        "m",
        rpi_ai::types::Api::Faux,
        "review183",
        "https://example.invalid",
    );
    config.tool_execution = execution;
    let (_, messages) = tokio::time::timeout(
        Duration::from_secs(10),
        common::run_and_collect(
            vec![common::user_message("counter fixture")],
            AgentContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![Arc::new(tool)],
            },
            config,
            stream_fn,
        ),
    )
    .await
    .unwrap();
    let text = messages
        .iter()
        .find_map(|m| match m {
            rpi_agent::AgentMessage::ToolResult(r) => Some(common::tool_result_text(r)),
            _ => None,
        })
        .unwrap();
    let result = (
        calls.load(Ordering::SeqCst),
        streamed_marker.load(Ordering::SeqCst),
        final_marker.load(Ordering::SeqCst),
        text,
    );
    eprintln!("mode={mode:?} execution={execution:?} => {result:?}");
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_ordinary_control_executes_once() {
    for execution in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
        assert_eq!(
            observe(Mode::Ordinary, execution).await,
            (1, false, false, "local-only".into())
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_recording_in_both_outputs_prevents_execution() {
    for execution in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
        assert_eq!(
            observe(Mode::Both, execution).await,
            (0, true, true, "upstream-only".into())
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_recorded_end_must_not_become_executable_final_call() {
    let mut counts = vec![];
    for execution in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
        let observed = observe(Mode::EndOnly, execution).await;
        assert!(
            observed.1,
            "the recording reached the actual provider event stream"
        );
        counts.push(observed.0);
    }
    assert_eq!(
        counts,
        vec![0, 0],
        "documented recorded toolcall_end was erased at final out boundary"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_explicit_null_end_must_fail_closed_at_final_boundary() {
    let mut counts = vec![];
    for execution in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
        let observed = observe(Mode::NullEndOnly, execution).await;
        assert!(observed.1);
        counts.push(observed.0);
    }
    assert_eq!(
        counts,
        vec![0, 0],
        "malformed recorded marker must not fall back to executing the call"
    );
}
