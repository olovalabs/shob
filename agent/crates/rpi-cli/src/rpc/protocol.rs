//! Minimal P0 JSONL protocol for the native rpi RPC host.
use serde_json::{json, Value};

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const TOOL_PROGRAM_CHILD_EVENT_MAX_BYTES: usize =
    rpi_harness::session::session::NATIVE_JOURNAL_EVENT_MAX_BYTES;
pub const PI_BASELINE: &str = "1.2.0";

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub command: Command,
}

/// Payload schema for `Request`. Only deserialization matters: the dispatcher
/// matches on the raw `type` string, so the parsed fields are never read.
#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Prompt {
        #[serde(flatten)]
        content: PromptPayload,
    },
    Abort,
    GetState,
    /// Versioned, bounded exact startup resource observations.
    GetResourceState {
        version: Option<u32>,
    },
    /// List extension slash commands (name and description).
    GetCommands,
    /// Invoke an extension handler and return its JSON result as response data.
    /// `command` accepts a registered name with or without a leading slash.
    ExecuteCommand {
        command: String,
        #[serde(default)]
        args: String,
    },
    /// Read-only identity snapshot for ToolProgram live reconciliation.
    GetToolProgramBranch,
    /// Read-only snapshot of the current branch's message entries.
    GetMessages {
        limit: Option<u64>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
    },
    /// Optional bounded user summaries and transcript windows.
    GetMessageSummaries {
        limit: Option<u64>,
        role: Option<String>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    GetMessageWindow {
        limit: Option<u64>,
        #[serde(rename = "anchorId")]
        anchor_id: Option<String>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
        #[serde(rename = "afterId")]
        after_id: Option<String>,
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    /// Strict typed queries are validated after stripping only the RPC envelope.
    GetToolProgramChildren,
    GetToolProgramChildPhases,
    GetToolProgramChildBody,
    GetHistory {
        limit: Option<u64>,
        #[serde(rename = "afterSeq")]
        after_seq: Option<u64>,
        #[serde(rename = "beforeSeq")]
        before_seq: Option<u64>,
    },
    /// Query custom entries on the selected branch. Query validation is shared
    /// with the runtime action (required non-empty `customTypes`; optional
    /// `afterSeq`, `limit`, `order`), after removing the RPC envelope.
    GetSessionBranch,
    /// Append a protocol-neutral custom entry to the current branch.
    AppendEntry {
        #[serde(rename = "customType")]
        custom_type: String,
        data: Option<Value>,
    },
    GetStats,
    GetQueue,
    QueueMessage {
        message: String,
        #[serde(rename = "deliverAs")]
        deliver_as: Option<String>,
    },
    CancelQueued {
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    ClearQueue,
    Navigate {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
    /// Continue the run that recovery left interrupted.
    ResumePending,
    /// Rewrite the model's view of the branch into a summary.
    Compact {
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    GetAvailableModels,
    GetAuthStatus,
    #[serde(rename = "oauth")]
    OAuth {
        provider: String,
        action: String,
        input: Option<Value>,
    },
    SetApiKey {
        provider: String,
        #[serde(rename = "apiKey")]
        api_key: String,
    },
    SetModel {
        provider: Option<String>,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    GetAvailableThinkingLevels,
    SetThinkingLevel {
        level: rpi_ai::types::ThinkingLevel,
    },
    ConfigureFileIo {
        #[serde(rename = "readText")]
        read_text: bool,
        #[serde(rename = "writeText")]
        write_text: bool,
    },
    FileIoResponse {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    ConfigureExternalTools {
        tools: Vec<rpi_tools::tools::external::ExternalToolDefinition>,
    },
    // Outcome validation is presence-aware in ExternalTools::respond: Option
    // would conflate explicit null with an absent outcome.
    ExternalToolResponse {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    GetAvailableTools,
    SetActiveTools {
        tools: Vec<String>,
    },
    UiDialogResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        answer: Option<Value>,
        #[serde(default)]
        cancelled: bool,
    },
    Ping,
    Stop,
}

/// Shape-only deserialization; pixel validation belongs to the owned run worker.
/// Presence, not serde aliases, determines legacy precedence.
#[allow(dead_code)] // Shape-only schema; production dispatch uses the owned raw frame.
#[derive(Debug)]
pub enum PromptPayload {
    Legacy(String),
    Blocks(Vec<Value>),
}
impl<'de> serde::Deserialize<'de> for PromptPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
        let invalid = |error: String| <D::Error as serde::de::Error>::custom(error);
        super::prompt_content::validate_prompt_shape(&value).map_err(invalid)?;
        if let Some(blocks) = value.get("contentBlocks") {
            Ok(Self::Blocks(
                blocks.as_array().expect("validated blocks").clone(),
            ))
        } else {
            Ok(Self::Legacy(
                value
                    .get("message")
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("prompt"))
                    .and_then(Value::as_str)
                    .expect("validated legacy string")
                    .to_owned(),
            ))
        }
    }
}

pub const SUPPORTED_EVENTS: &[&str] = &[
    "session_entry_committed",
    "external_tool_request",
    "external_tool_request_closed",
    "file_io_request",
    "file_io_request_closed",
    "tool_program_child_begin",
    "tool_program_child_phase",
    "tool_program_child_resync_required",
];

pub const SUPPORTED_COMMANDS: &[&str] = &[
    "get_resource_state",
    "get_state",
    "get_commands",
    "execute_command",
    "get_tool_program_branch",
    "get_messages",
    "get_message_summaries",
    "get_message_window",
    "get_tool_program_children",
    "get_tool_program_child_phases",
    "get_tool_program_child_body",
    "get_history",
    "get_session_branch",
    "append_entry",
    "get_stats",
    "get_queue",
    "queue_message",
    "cancel_queued",
    "clear_queue",
    "navigate",
    "prompt",
    "resume_pending",
    "compact",
    "abort",
    "get_available_models",
    "get_auth_status",
    "get_provider_models",
    "test_provider_config",
    "oauth",
    "set_api_key",
    "set_provider_config",
    "update_provider_config",
    "delete_provider_config",
    "set_model",
    "get_available_thinking_levels",
    "set_thinking_level",
    "configure_file_io",
    "file_io_response",
    "configure_external_tools",
    "external_tool_response",
    "get_available_tools",
    "set_active_tools",
    "ui_dialog_response",
    "ping",
    "stop",
];

pub fn response(id: Option<Value>, command: &str, result: Result<Option<Value>, String>) -> Value {
    let mut out = json!({
        "type": "response",
        "command": command,
        "success": result.is_ok(),
        // Keep the original remote-client response fields during the RPC
        // protocol transition. New clients use success/data; older clients
        // still require status/result to deserialize the response.
        "status": if result.is_ok() { "ok" } else { "error" },
    });
    if let Some(id) = id {
        out["id"] = id;
    }
    match result {
        Ok(Some(data)) => {
            out["data"] = data.clone();
            out["result"] = data;
        }
        Ok(None) => {}
        Err(error) => out["error"] = json!(error),
    }
    out
}

pub fn ready(mode: crate::args::RpcMessageUpdates) -> Value {
    json!({
        "type": "ready",
        "protocol": "rpi",
        "piCompatibilityTarget": PI_BASELINE,
        "supportedCommands": SUPPORTED_COMMANDS,
        "supportedEvents": SUPPORTED_EVENTS,
        "promptContent": super::prompt_content::prompt_capabilities(),
        "fileIO": super::file_io::capabilities(),
        "toolProgramChildren": {
            "version": 1,
            "summaryCommand": "get_tool_program_children",
            "phaseCommand": "get_tool_program_child_phases",
            "bodyCommand": "get_tool_program_child_body",
            "maxCount": rpi_harness::tool_program_queries::MAX_COUNT,
            "maxPhaseCandidates": rpi_harness::tool_program_queries::MAX_PHASE_CANDIDATES,
            // This bounds the serialized native data page, not the RPC frame:
            // response() duplicates data into result for compatibility.
            "maxResponseBytes": rpi_harness::tool_program_queries::MAX_RESPONSE_BYTES,
            "responseBytesScope": "pageBytes",
            "maxBodyDecodedBytes": rpi_harness::tool_program_body::MAX_PAGE_BYTES,
            "summaryState": "unknown",
            "recovery": "readOnly"
        },
        "toolProgramChildEvents": {
            "version": 1,
            "beginEvent": "tool_program_child_begin",
            "phaseEvent": "tool_program_child_phase",
            "resyncEvent": "tool_program_child_resync_required",
            "sequence": "nativeEntrySeq-v1",
            "delivery": "bestEffortResync",
            "maxEventBytes": TOOL_PROGRAM_CHILD_EVENT_MAX_BYTES
        },
        "toolProgramBranch": {
            "version": 1,
            "command": "get_tool_program_branch",
            "identity": "bindingAndLaneGeneration"
        },
        "semantics": {
            "messageUpdates": mode.as_str(),
            "messageIds": "sessionEntryId-v1",
            "promptResponse": "accepted",
            "settlementEvent": "agent_settled",
            "agentEndIsTerminal": false
        },
        "limits": {"frameBytes": MAX_FRAME_BYTES, "maxConcurrentPrompts": 1},
        "parityComplete": false
    })
}

/// Only expose capability metadata, never provider credentials or headers.
pub fn model_descriptor(model: &rpi_ai::Model) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "api": model.api,
        "provider": model.provider,
        "baseUrl": model.base_url,
        "reasoning": model.reasoning,
        "input": model.input,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_commands_are_unique() {
        let commands: std::collections::HashSet<_> = SUPPORTED_COMMANDS.iter().collect();
        assert_eq!(commands.len(), SUPPORTED_COMMANDS.len());
    }

    #[test]
    fn child_inspection_capability_is_exact_v1_and_advertises_only_read_commands() {
        let ready = ready(crate::args::RpcMessageUpdates::default());
        let capability = &ready["toolProgramChildren"];
        assert_eq!(
            capability,
            &json!({
                "version": 1,
                "summaryCommand": "get_tool_program_children",
                "phaseCommand": "get_tool_program_child_phases",
                "bodyCommand": "get_tool_program_child_body",
                "maxCount": 32,
                "maxPhaseCandidates": 128,
                "maxResponseBytes": 131072,
                "responseBytesScope": "pageBytes",
                "maxBodyDecodedBytes": 65536,
                "summaryState": "unknown",
                "recovery": "readOnly"
            })
        );
        for (key, matches) in [
            ("summaryCommand", 0),
            ("phaseCommand", 1),
            ("bodyCommand", 2),
        ] {
            let name = capability[key].as_str().unwrap();
            assert!(SUPPORTED_COMMANDS.contains(&name));
            let request: Request =
                serde_json::from_value(json!({"type": name, "id": "req"})).unwrap();
            assert_eq!(request.id, Some(json!("req")));
            assert!(match (matches, request.command) {
                (0, Command::GetToolProgramChildren)
                | (1, Command::GetToolProgramChildPhases)
                | (2, Command::GetToolProgramChildBody) => true,
                _ => false,
            });
        }
        for name in ["get_tool_program_children_v2", "replay_tool_program_child"] {
            assert!(!SUPPORTED_COMMANDS.contains(&name));
            assert!(serde_json::from_value::<Request>(json!({"type": name})).is_err());
        }
        assert!(capability.get("events").is_none());
        let events = &ready["toolProgramChildEvents"];
        assert_eq!(
            events,
            &json!({
                "version": 1,
                "beginEvent": "tool_program_child_begin",
                "phaseEvent": "tool_program_child_phase",
                "resyncEvent": "tool_program_child_resync_required",
                "sequence": "nativeEntrySeq-v1",
                "delivery": "bestEffortResync",
                "maxEventBytes": 32 * 1024
            })
        );
        for event in [
            "tool_program_child_begin",
            "tool_program_child_phase",
            "tool_program_child_resync_required",
        ] {
            assert!(ready["supportedEvents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|advertised| advertised == event));
        }
        assert_eq!(ready["limits"]["frameBytes"], MAX_FRAME_BYTES);
    }

    #[test]
    fn child_page_budget_does_not_claim_to_bound_the_compatibility_response_frame() {
        let data = json!({"body": "x".repeat(100_000)});
        assert!(serde_json::to_vec(&data).unwrap().len() <= 131072);
        let frame = response(
            Some(json!("req")),
            "get_tool_program_child_body",
            Ok(Some(data)),
        );
        let bytes = serde_json::to_vec(&frame).unwrap().len();
        assert!(bytes > 131072);
        assert!(bytes < MAX_FRAME_BYTES);
        assert_eq!(frame["data"], frame["result"]);
    }

    #[test]
    fn extension_commands_are_advertised_and_args_default_to_empty() {
        for name in ["get_commands", "execute_command"] {
            assert!(SUPPORTED_COMMANDS.contains(&name));
        }
        let request: Request = serde_json::from_value(json!({
            "type": "execute_command", "command": "/example"
        }))
        .unwrap();
        assert!(
            matches!(request.command, Command::ExecuteCommand { command, args }
            if command == "/example" && args.is_empty())
        );
        assert!(serde_json::from_value::<Request>(json!({
            "type": "execute_command", "command": "example", "args": 42
        }))
        .is_err());
    }

    #[test]
    fn ready_advertises_the_p0_contract() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            assert_eq!(ready(mode)["semantics"]["messageUpdates"], mode.as_str());
        }
        let value = ready(crate::args::RpcMessageUpdates::default());
        assert_eq!(value["protocol"], "rpi");
        assert_eq!(value["semantics"]["messageIds"], "sessionEntryId-v1");
        assert_eq!(value["semantics"]["settlementEvent"], "agent_settled");
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "prompt"));
    }

    #[test]
    fn ready_advertises_the_read_only_history_snapshot() {
        let value = ready(crate::args::RpcMessageUpdates::default());
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "get_messages"));
    }

    #[test]
    fn ready_advertises_resuming_an_interrupted_run() {
        let value = ready(crate::args::RpcMessageUpdates::default());
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "resume_pending"));
    }

    #[test]
    fn ready_advertises_compaction() {
        let value = ready(crate::args::RpcMessageUpdates::default());
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "compact"));
    }

    #[test]
    fn compact_payload_carries_instructions() {
        let request: Request = serde_json::from_value(json!({
            "type": "compact", "id": 5, "customInstructions": "keep the decisions"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::Compact {
                custom_instructions: Some(_)
            }
        ));
    }

    #[test]
    fn resume_pending_is_a_bare_command() {
        let request: Request =
            serde_json::from_value(json!({"type": "resume_pending", "id": 4})).unwrap();
        assert!(matches!(request.command, Command::ResumePending));
    }

    #[test]
    fn get_messages_payload_keeps_its_limit_and_cursor() {
        let request: Request = serde_json::from_value(json!({
            "type": "get_messages", "id": 3, "limit": 25, "beforeId": "entry-12"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::GetMessages {
                limit: Some(25),
                before_id: Some(_)
            }
        ));
    }

    #[test]
    fn auth_commands_keep_secret_fields_in_request_only() {
        let request: Request = serde_json::from_value(json!({
            "type": "set_api_key",
            "id": 8,
            "provider": "anthropic",
            "apiKey": "secret-fixture"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::SetApiKey { provider, api_key }
                if provider == "anthropic" && api_key == "secret-fixture"
        ));
        let status = response(
            Some(json!(8)),
            "set_api_key",
            Ok(Some(json!({
                "provider": "anthropic",
                "ready": true,
                "source": "auth.json",
                "requiresRestart": true
            }))),
        );
        assert!(!status.to_string().contains("secret-fixture"));
    }
    #[test]
    fn prompt_payload_accepts_legacy_content_and_prompt_fields() {
        for field in ["content", "prompt"] {
            let request: Request = serde_json::from_value(json!({
                "type": "prompt",
                field: "legacy text"
            }))
            .unwrap();
            assert!(
                matches!(request.command, Command::Prompt { content: PromptPayload::Legacy(message) } if message == "legacy text")
            );
        }
    }

    #[test]
    fn oauth_payload_keeps_provider_action_and_input() {
        let request: Request = serde_json::from_value(json!({
            "type": "oauth",
            "id": 9,
            "provider": "example",
            "action": "begin",
            "input": {"code": "fixture"}
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::OAuth { provider, action, input: Some(input) }
                if provider == "example"
                    && action == "begin"
                    && input["code"] == "fixture"
        ));
    }

    #[test]
    fn response_has_canonical_success_data_shape() {
        let value = response(
            Some(json!(7)),
            "prompt",
            Ok(Some(json!({"accepted": true}))),
        );
        assert_eq!(value["type"], "response");
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["data"]["accepted"], true);
        assert_eq!(value["result"]["accepted"], true);
        let legacy: crate::remote::protocol::RemoteResponse =
            serde_json::from_value(value).unwrap();
        assert!(legacy.is_ok());
        assert_eq!(legacy.result.unwrap()["accepted"], true);
    }
}
