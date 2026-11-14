use super::*;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::{StopReason, Tool};
fn invoke(bridge: &Arc<ActionBridge>, action: RuntimeActionId, args: Value) -> (i32, Value) {
    let mut out = StbString::empty();
    let text = args.to_string();
    let rc = trampoline_runtime_action(
        action.into(),
        StbStringRef::from_str(&text),
        &mut out,
        bridge.user_data(),
    );
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
    };
    (rc, serde_json::from_str(&owned.to_string_lossy()).unwrap())
}
struct Probe {
    schema: Tool,
    bridge: Arc<ActionBridge>,
}
#[async_trait::async_trait]
impl AgentTool for Probe {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "probe"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let driver_signal = signal.child_token();
        let invocation = self.bridge.for_invocation(true, driver_signal.clone());
        let registration = self.bridge.clone();
        tokio::task::spawn_blocking(move || {
            let op = RuntimeActionId::ToolProgram;
            assert!(registration.resolve_content_parts(vec![]).is_err());
            assert!(invocation.resolve_content_parts(vec![]).is_ok());
            assert_eq!(invoke(&registration, op, json!({"op":"list"})).0, 1);
            assert_eq!(invoke(&invocation, op, json!({"op":"list"})).0, 1);
            assert_eq!(
                invoke(
                    &invocation,
                    op,
                    json!({"op":"list","__rpiRuntimeKey":"forged"})
                )
                .0,
                1
            );
            let key = invocation.invocation_key().unwrap();
            let (rc, result) = invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key}));
            assert_eq!(rc, 0);
            assert_eq!(result["protocolVersion"], 1);
            assert_eq!(result["tools"], json!([]));
            // Names-only legacy action is scoped too; it must not reach this mock's root getter.
            assert_eq!(
                invoke(&invocation, RuntimeActionId::GetActiveTools, json!({})).1,
                json!({"tools":["codemode"]})
            );
            {
                let _guard = invocation.enter_invocation();
                assert_eq!(invoke(&registration, op, json!({"op":"list"})).0, 0);
            }
            driver_signal.cancel();
            assert!(invocation.resolve_content_parts(vec![]).is_err());
            assert_eq!(
                invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key})).0,
                1
            );
            // Cleanup is still authenticated and available after cancellation.
            assert_eq!(
                invoke(
                    &invocation,
                    op,
                    json!({"op":"dispose","requestId":"unused","__rpiRuntimeKey":key})
                )
                .0,
                0
            );
            registration.invalidate();
            assert!(invocation.resolve_content_parts(vec![]).is_err());
            assert_eq!(
                invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key})).0,
                1
            );
        })
        .await
        .unwrap();
        Ok(AgentToolResult::text("done"))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_program_authentication_and_narrowed_ffi_binding() {
    let bridge =
        ActionBridge::new(Handle::current(), Arc::new(Host::default())).for_plugin("codemode");
    let schema: Tool = serde_json::from_value(
        json!({"name":"codemode","description":"program","parameters":{"type":"object"}}),
    )
    .unwrap();
    let context = rpi_agent::AgentContext {
        tools: vec![Arc::new(Probe { schema, bridge })],
        ..Default::default()
    };
    let (_, messages) = agent_common::run_and_collect(
        vec![agent_common::user_message("run")],
        context,
        agent_common::base_config(),
        agent_common::mock_stream_fn(vec![
            agent_common::assistant_tool_calls(
                vec![("p", "codemode", json!({}))],
                StopReason::ToolUse,
            ),
            agent_common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m,rpi_agent::AgentMessage::ToolResult(r) if r.is_error)),
        "{messages:?}"
    );
}

// Run a real native invocation body around the ABI adapter. Its captured
// ToolProgram must cross spawn_blocking, and resolution must precede destroy.
use crate::tool::{PluginToolAdapter, PluginToolHandle};
use rpi_agent::ToolExecutionMode;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};
struct ReceiptDrive(Value);
extern "C" fn receipt_execute(
    _: StbStringRef,
    args: StbString,
    free: Option<rpi_plugin_sdk::FreeStringFn>,
) -> StepHandle {
    let owned = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(args, free) };
    let args: Value = serde_json::from_str(&owned.to_string_lossy()).unwrap();
    Box::into_raw(Box::new(ReceiptDrive(args))) as StepHandle
}
extern "C" fn receipt_poll(
    handle: StepHandle,
    _: Option<ToolPartialCb>,
    _: *mut c_void,
) -> StepResult {
    let drive = unsafe { &*(handle as *const ReceiptDrive) };
    if let Some(ids) = drive.0.get("resultIds").and_then(Value::as_array) {
        let bridge = ActionBridge::resolve_handle(
            drive.0["__rpi"]["runtimeHandle"].as_u64().unwrap() as usize,
        )
        .unwrap();
        let (rc, measured) = invoke(
            &bridge,
            RuntimeActionId::ToolProgram,
            json!({"op":"measure_result_content","resultId":ids[0],"__rpiRuntimeKey":drive.0["__rpi"]["runtimeKey"]}),
        );
        assert_eq!(rc, 0);
        assert_eq!(measured["contentBlocks"], 3);
        assert!(measured["encodedBytes"].as_u64().unwrap() > 0);
        assert_eq!(measured.as_object().unwrap().len(), 3);
    }
    StepResult::done(StbString::from_string(drive.0["done"].to_string()))
}
extern "C" fn receipt_cancel(_: StepHandle) {}
extern "C" fn receipt_waker(_: StepHandle, _: Option<rpi_plugin_sdk::ToolWakeCb>, _: *mut c_void) {}
extern "C" fn receipt_destroy(handle: StepHandle) {
    let drive = unsafe { Box::from_raw(handle as *mut ReceiptDrive) };
    let args = drive.0;
    let bridge =
        ActionBridge::resolve_handle(args["__rpi"]["runtimeHandle"].as_u64().unwrap() as usize)
            .unwrap();
    let ids = args
        .get("resultIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![args["resultId"].clone()]);
    for id in ids {
        let (rc, _) = invoke(
            &bridge,
            RuntimeActionId::ToolProgram,
            json!({"op":"release_result","resultId":id,"__rpiRuntimeKey":args["__rpi"]["runtimeKey"]}),
        );
        assert_eq!(
            rc, 0,
            "destroy must still have authenticated cleanup access"
        );
    }
}
struct ReceiptParent {
    schema: Tool,
    bridge: Arc<ActionBridge>,
    wake: bool,
    error: bool,
    invalid: u8,
}
#[async_trait::async_trait]
impl AgentTool for ReceiptParent {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "receipt parent"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let p = rpi_agent::tool_program::current().unwrap();
        let id = p
            .handle(json!({"op":"start","name":"effect","arguments":{},"resultMode":"reference"}))
            .await
            .unwrap()["requestId"]
            .clone();
        loop {
            let reply = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if reply["status"] != "running" {
                assert_eq!(reply["status"], "done");
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            self.bridge
                .resolve_result_reference(id.as_str().unwrap())
                .is_err(),
            "registration cannot select receipts"
        );
        let mut done = json!({"$rpiToolResult":1,"isError":self.error,"resultReference":{"version":1,"id":id}});
        match self.invalid {
            1 => done["isError"] = json!(!self.error),
            2 => done["result"] = json!({}),
            3 => done["resultReference"]["version"] = json!(2),
            4 => done["resultReference"]["id"] = json!("tp-foreign"),
            5 => done["resultReference"] = json!(null),
            6 => done["resultReference"]["id"] = json!(7),
            7 => done["$rpiToolResult"] = json!(2),
            _ => {}
        }
        let adapter = PluginToolAdapter::new(
            self.schema.clone(),
            PluginToolHandle {
                execute_fn: receipt_execute,
                set_waker_fn: self.wake.then_some(receipt_waker),
                poll_fn: receipt_poll,
                cancel_fn: receipt_cancel,
                destroy_fn: receipt_destroy,
                plugin_free_string: crate::host_free_string,
                runtime_handle: Some(self.bridge.user_data() as usize),
            },
            crate::loader::PluginKeepalive::empty(),
        );
        let result = adapter
            .execute(
                "native-parent",
                json!({"done":done,"resultId":id}),
                signal,
                update,
            )
            .await;
        assert!(
            p.resolve_result_reference(id.as_str().unwrap()).is_err(),
            "destroy released receipt after resolution"
        );
        if self.invalid != 0 {
            assert!(matches!(result, Err(AgentError::Tool(_))), "{result:?}");
        }
        result
    }
}
struct ReceiptEffect {
    schema: Tool,
    error: bool,
    empty: bool,
}
#[async_trait::async_trait]
impl AgentTool for ReceiptEffect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "native receipt"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let mut usage = rpi_ai::types::Usage::zero();
        usage.total_tokens = 3;
        let native = AgentToolResult {
            content: if self.empty {
                vec![]
            } else {
                vec![rpi_agent::TextContentOrImage::Text(serde_json::from_value(json!({"type":"text","text":"native 😀".repeat(200_000),"textSignature":"native-signature"})).unwrap())]
            },
            details: json!({"business":{"runtimeKey":"customer-owned"},"large":"é".repeat(600_000)}),
            usage: Some(usage),
            added_tool_names: vec!["native-added".into()],
            terminate: true,
        };
        if self.error {
            Err(AgentError::StructuredTool(Box::new(native)))
        } else {
            Ok(native)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_receipt_attachment_crosses_both_drivers_before_destroy_with_authoritative_accounting(
) {
    for wake in [false, true] {
        for (error, empty, invalid) in [
            (false, false, 0),
            (true, false, 0),
            (false, true, 0),
            (true, true, 0),
            (false, true, 1),
            (false, true, 2),
            (false, true, 3),
            (false, true, 4),
            (false, true, 5),
            (false, true, 6),
            (false, true, 7),
        ] {
            let bridge = ActionBridge::new(Handle::current(), Arc::new(Host::default()))
                .for_plugin("receipt");
            let schema: Tool =
                serde_json::from_value(json!({"name":"codemode","description":"program",
                "parameters":{"type":"object","x-rpi-result-envelope":1}}))
                .unwrap();
            let effect = ReceiptEffect {
                schema: serde_json::from_value(
                    json!({"name":"effect","description":"native","parameters":{"type":"object"}}),
                )
                .unwrap(),
                error,
                empty,
            };
            let context = rpi_agent::AgentContext {
                tools: vec![
                    Arc::new(ReceiptParent {
                        schema,
                        bridge,
                        wake,
                        error,
                        invalid,
                    }),
                    Arc::new(effect),
                ],
                ..Default::default()
            };
            let (events, messages) = agent_common::run_and_collect(
                vec![agent_common::user_message("run")],
                context,
                agent_common::base_config(),
                agent_common::mock_stream_fn(vec![
                    agent_common::assistant_tool_calls(
                        vec![("p", "codemode", json!({}))],
                        StopReason::ToolUse,
                    ),
                    agent_common::assistant_text("done", StopReason::Stop),
                ]),
            )
            .await;
            let results: Vec<_> = messages
                .iter()
                .filter_map(|m| match m {
                    rpi_agent::AgentMessage::ToolResult(r) => Some(r),
                    _ => None,
                })
                .collect();
            assert_eq!(
                results.len(),
                1,
                "receipts cannot forge transcript tool calls"
            );
            let result = results[0];
            assert_eq!(result.is_error, error || invalid != 0);
            assert_eq!(
                result.usage.as_ref().unwrap().total_tokens,
                3,
                "no attachment double count"
            );
            assert_eq!(result.added_tool_names, vec!["native-added"]);
            let end = events
                .iter()
                .find_map(|e| match e {
                    rpi_agent::AgentEvent::ToolExecutionEnd { result, .. } => Some(result),
                    _ => None,
                })
                .unwrap();
            assert!(
                end.terminate,
                "finalized termination survives selection failure"
            );
            if invalid == 0 {
                assert_eq!(end.details["business"]["runtimeKey"], "customer-owned");
                assert_eq!(result.content.len(), if empty { 0 } else { 1 });
                if !empty {
                    let block = serde_json::to_value(&result.content[0]).unwrap();
                    assert_eq!(block["textSignature"], "native-signature");
                    assert!(block["text"].as_str().unwrap().len() > 1024 * 1024);
                }
            }
        }
    }
}

struct ContentEffect {
    schema: Tool,
    native: AgentToolResult,
}
#[async_trait::async_trait]
impl AgentTool for ContentEffect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "content source"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        Err(AgentError::StructuredTool(Box::new(self.native.clone())))
    }
}
struct ContentParent {
    schema: Tool,
    bridge: Arc<ActionBridge>,
    wake: bool,
    case: u8,
}
#[async_trait::async_trait]
impl AgentTool for ContentParent {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "content parent"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let p = rpi_agent::tool_program::current().unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let id = p
                .handle(
                    json!({"op":"start","name":"effect","arguments":{},"resultMode":"reference"}),
                )
                .await
                .unwrap()["requestId"]
                .clone();
            loop {
                let reply = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
                if reply["status"] != "running" {
                    assert_eq!(reply["status"], "done");
                    break;
                }
                tokio::task::yield_now().await;
            }
            ids.push(id);
        }
        assert!(self.bridge.resolve_content_parts(vec![]).is_err());
        let mut usage = rpi_ai::types::Usage::zero();
        usage.total_tokens = 5;
        let mut done = json!({"$rpiToolResult":1,"isError":self.case==25,
        "result":{"details":{"outer":"retained"},"terminate":false,"usage":usage,"addedToolNames":["outer-added"]},
        "contentParts":[
            {"content":[{"type":"text","text":"prefix","textSignature":"inline-signature"}]},
            {"reference":{"version":1,"id":ids[0]},"indices":[2,0,1,0]},
            {"content":[{"type":"text","text":"suffix"}]},
            {"reference":{"version":1,"id":ids[1]},"indices":[1]},
            {"reference":{"version":1,"id":ids[0]},"indices":[]}
        ]});
        match self.case {
            1 => done["result"]["content"] = json!([]),
            2 => done["resultReference"] = json!({"version":1,"id":ids[0]}),
            3 => done["contentParts"][0]["extra"] = json!(true),
            4 => done["contentParts"][1]["reference"]["extra"] = json!(1),
            5 => done["contentParts"][1]["reference"]["version"] = json!(1.0),
            6 => done["contentParts"][1]["reference"]["id"] = json!(""),
            7 => done["contentParts"][1]["indices"] = json!([1.0]),
            8 => done["contentParts"][1]["indices"] = json!([-1]),
            9 => done["contentParts"][1]["indices"] = json!([3]),
            10 => done["contentParts"][1]["reference"]["id"] = json!("tp-foreign"),
            11 => {
                p.handle(json!({"op":"release_result","resultId":ids[0]}))
                    .await
                    .unwrap();
                done["contentParts"][1]["indices"] = json!([]);
            }
            12 => done["contentParts"] = json!(vec![json!({"content":[]}); 257]),
            13 => done["contentParts"][1]["content"] = json!([]),
            14 => {
                done["result"].as_object_mut().unwrap().remove("details");
            }
            15 => done["result"]["terminate"] = json!(null),
            16 => done["result"]["usage"] = json!({}),
            17 => done["result"]["addedToolNames"] = json!([1]),
            18 => done["contentParts"][0]["content"] = json!([{"type":"text","text":null}]),
            19 => {
                done["contentParts"][0]["content"] =
                    json!([{"type":"text","text":"x","textSignature":12}])
            }
            20 => {
                done["contentParts"][0]["content"] =
                    json!([{"type":"image","mimeType":"image/svg+xml","data":"AA=="}])
            }
            21 => {
                done["contentParts"][0]["content"] =
                    json!([{"type":"image","mimeType":"image/png","data":"AB=="}])
            }
            22 => done["contentParts"][1]["indices"] = json!(vec![0; 257]),
            23 => {
                done["contentParts"] = json!([{"reference":{"version":1,"id":ids[0]},"indices":vec![0;256]},{"content":[{"type":"text","text":"overflow"}]}])
            }
            24 => done["contentParts"] = json!([{"content":[]}]),
            25 => {
                done["contentParts"] = json!([{"reference":{"version":1,"id":ids[0]},"indices":null},{"reference":{"version":1,"id":ids[1]}}])
            }
            26 => {
                done["contentParts"] =
                    json!([{"reference":{"version":1,"id":ids[0]},"indices":vec![0;128]}])
            }
            _ => {}
        }
        let adapter = PluginToolAdapter::new(
            self.schema.clone(),
            PluginToolHandle {
                execute_fn: receipt_execute,
                set_waker_fn: self.wake.then_some(receipt_waker),
                poll_fn: receipt_poll,
                cancel_fn: receipt_cancel,
                destroy_fn: receipt_destroy,
                plugin_free_string: crate::host_free_string,
                runtime_handle: Some(self.bridge.user_data() as usize),
            },
            crate::loader::PluginKeepalive::empty(),
        );
        let cleanup_ids: Vec<_> = ids
            .iter()
            .enumerate()
            .filter(|(i, _)| self.case != 11 || *i != 0)
            .map(|(_, id)| id.clone())
            .collect();
        let result = adapter
            .execute(
                "native-parent",
                json!({"done":done,"resultIds":cleanup_ids}),
                signal,
                update,
            )
            .await;
        for id in ids {
            assert!(
                p.resolve_result_reference(id.as_str().unwrap()).is_err(),
                "destroy releases all sources after native resolution"
            );
        }
        if !matches!(self.case, 0 | 24 | 25) {
            assert!(
                matches!(result, Err(AgentError::Tool(_))),
                "case {}: {result:?}",
                self.case
            );
        }
        result
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_content_plans_resolve_before_destroy_in_both_drivers() {
    for wake in [false, true] {
        for case in 0..=26 {
            let large = case == 0 || case == 26;
            let text = if large {
                "native 😀".repeat(200_000)
            } else {
                "native 😀".into()
            };
            // Real PNG/base64 is opaque to native content composition.
            let image = if large {
                large_png_base64()
            } else {
                "YWJj".into()
            };
            let mut usage = rpi_ai::types::Usage::zero();
            usage.total_tokens = 3;
            let native = AgentToolResult {
                content: vec![
                    rpi_agent::TextContentOrImage::Text(
                        serde_json::from_value(
                            json!({"type":"text","text":text,"textSignature":"native-signature"}),
                        )
                        .unwrap(),
                    ),
                    rpi_agent::TextContentOrImage::Image(
                        serde_json::from_value(
                            json!({"type":"image","mimeType":"image/png","data":image}),
                        )
                        .unwrap(),
                    ),
                    rpi_agent::TextContentOrImage::text("tail"),
                ],
                details: json!({"child":"must not escape"}),
                usage: Some(usage),
                added_tool_names: vec!["native-added".into()],
                terminate: false,
            };
            let bridge = ActionBridge::new(Handle::current(), Arc::new(Host::default()))
                .for_plugin("receipt");
            let context=rpi_agent::AgentContext {tools:vec![
                Arc::new(ContentParent {schema:serde_json::from_value(json!({"name":"codemode","description":"program","parameters":{"type":"object","x-rpi-result-envelope":1}})).unwrap(),bridge,wake,case}),
                Arc::new(ContentEffect {schema:serde_json::from_value(json!({"name":"effect","description":"source","parameters":{"type":"object"}})).unwrap(),native}),
            ],..Default::default()};
            let (events, messages) = agent_common::run_and_collect(
                vec![agent_common::user_message("run")],
                context,
                agent_common::base_config(),
                agent_common::mock_stream_fn(vec![
                    agent_common::assistant_tool_calls(
                        vec![("p", "codemode", json!({}))],
                        StopReason::ToolUse,
                    ),
                    agent_common::assistant_text("done", StopReason::Stop),
                ]),
            )
            .await;
            let results: Vec<_> = messages
                .iter()
                .filter_map(|m| {
                    if let rpi_agent::AgentMessage::ToolResult(r) = m {
                        Some(r)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(results.len(), 1);
            let result = results[0];
            let valid = matches!(case, 0 | 24 | 25);
            assert_eq!(
                result.is_error,
                !valid || case == 25,
                "wake={wake} case={case}"
            );
            assert_eq!(
                result.usage.as_ref().unwrap().total_tokens,
                if valid { 11 } else { 6 },
                "no child accounting replay"
            );
            let end = events
                .iter()
                .find_map(|e| {
                    if let rpi_agent::AgentEvent::ToolExecutionEnd { result, .. } = e {
                        Some(result)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert!(!end.terminate);
            if valid {
                assert_eq!(end.details, json!({"outer":"retained"}));
                assert_eq!(result.added_tool_names, vec!["native-added", "outer-added"]);
                assert_eq!(
                    result.content.len(),
                    match case {
                        24 => 0,
                        25 => 6,
                        _ => 7,
                    }
                );
                if case == 0 {
                    let blocks: Vec<_> = result
                        .content
                        .iter()
                        .map(|b| serde_json::to_value(b).unwrap())
                        .collect();
                    assert_eq!(blocks[0]["textSignature"], "inline-signature");
                    assert_eq!(blocks[1]["text"], "tail");
                    assert_eq!(blocks[2]["text"], text);
                    assert_eq!(blocks[2]["textSignature"], "native-signature");
                    assert_eq!(blocks[3]["data"], image);
                    assert_eq!(blocks[4], blocks[2]);
                    assert_eq!(blocks[5]["text"], "suffix");
                    assert_eq!(blocks[6], blocks[3]);
                }
            }
        }
    }
}

/// Valid one-pixel RGBA PNG with a large ancillary tEXt chunk, encoded without
/// adding image/base64 test dependencies or writing a fixture to the workspace.
fn large_png_base64() -> String {
    let mut png = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 122, 94, 171, 63,
    ];
    let mut text = b"note\0".to_vec();
    text.resize(900_000, b'x');
    png.extend_from_slice(&(text.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(b"tEXt");
    png.extend_from_slice(&text);
    let mut crc = !0u32;
    for byte in &png[start..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    png.extend_from_slice(&(!crc).to_be_bytes());
    png.extend_from_slice(&[0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130]);
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in png.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(alphabet[(a >> 2) as usize] as char);
        encoded.push(alphabet[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            alphabet[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            alphabet[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    assert!(encoded.len() > 1024 * 1024);
    encoded
}

#[tokio::test]
async fn content_parts_bridge_without_owning_program_fails_even_when_inline_only() {
    let bridge = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let invocation = bridge.for_invocation(false, CancellationToken::new());
    assert!(invocation
        .resolve_content_parts(vec![])
        .unwrap_err()
        .contains("effective ToolProgram"));
    assert!(invocation
        .resolve_content_parts(vec![
            rpi_agent::tool_program::ToolProgramContentPart::Inline(vec![
                rpi_agent::TextContentOrImage::text("inline")
            ])
        ])
        .is_err());
}
