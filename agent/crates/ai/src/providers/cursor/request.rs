use super::wire::{self, Proto, Result};
use crate::types::{Content, ToolCall, ToolResultMessage, UserContent};
use crate::{Context, Message, Model, SimpleStreamOptions};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub(super) type Blobs = HashMap<Vec<u8>, Vec<u8>>;
pub(super) fn setting<'a>(model: &'a Model, name: &str) -> Option<&'a Value> {
    model.sampling_params.as_ref()?.get(name)
}
fn store(blobs: &mut Blobs, bytes: Vec<u8>) -> Vec<u8> {
    let id = Sha256::digest(&bytes).to_vec();
    blobs.insert(id.clone(), bytes);
    id
}
fn store_json(blobs: &mut Blobs, value: Value) -> Result<Vec<u8>> {
    Ok(store(
        blobs,
        serde_json::to_vec(&value).map_err(|e| e.to_string())?,
    ))
}
fn normalized_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
fn root_user(content: &UserContent) -> Value {
    match content {
        UserContent::Text(t)=>json!([{"type":"text","text":t.trim()}]),
        UserContent::Blocks(blocks)=>Value::Array(blocks.iter().filter_map(|b|match b {
            Content::Text(t)=>Some(json!({"type":"text","text":t.text.trim()})),
            Content::Image(i)=>Some(json!({"type":"image","image":format!("data:{};base64,{}",i.mime_type,i.data),"mediaType":i.mime_type})),_=>None
        }).collect()),
    }
}
fn user_message(content: &UserContent) -> Result<Proto> {
    let (text, blocks) = match content {
        UserContent::Text(t) => (t.trim().to_owned(), &[][..]),
        UserContent::Blocks(b) => (Content::text_only(b, "\n"), b.as_slice()),
    };
    let mut selected = Proto::default();
    let mut has_images = false;
    for block in blocks {
        if let Content::Image(image) = block {
            let data = base64::engine::general_purpose::STANDARD
                .decode(image.data.as_bytes())
                .map_err(|e| format!("Invalid Cursor image: {e}"))?;
            selected = selected.msg(
                1,
                Proto::default()
                    .bytes(2, uuid::Uuid::new_v4().to_string())
                    .bytes(7, &image.mime_type)
                    .bytes(8, data),
            );
            has_images = true;
        }
    }
    let mut message = Proto::default()
        .bytes(1, text)
        .bytes(2, uuid::Uuid::new_v4().to_string());
    if has_images {
        message = message.msg(3, selected);
    }
    Ok(message)
}
fn mcp_args(call: &ToolCall) -> Proto {
    let mut args = Proto::default()
        .bytes(1, &call.name)
        .bytes(3, normalized_id(&call.id))
        .bytes(4, "pi-agent")
        .bytes(5, &call.name);
    if let Some(map) = call.arguments.as_object() {
        for (k, v) in map {
            args = args.msg(2, Proto::default().bytes(1, k).msg(2, wire::json_encode(v)));
        }
    }
    args
}
pub(super) fn mcp_result(result: &ToolResultMessage) -> Result<Proto> {
    if result.is_error {
        return Ok(Proto::default().msg(
            2,
            Proto::default().bytes(1, Content::text_only(&result.content, "\n")),
        ));
    }
    let mut success = Proto::default();
    for block in &result.content {
        let item = match block {
            Content::Text(t) => Proto::default().msg(1, Proto::default().bytes(1, &t.text)),
            Content::Image(i) => Proto::default().msg(
                2,
                Proto::default()
                    .bytes(
                        1,
                        base64::engine::general_purpose::STANDARD
                            .decode(i.data.as_bytes())
                            .map_err(|e| e.to_string())?,
                    )
                    .bytes(2, &i.mime_type),
            ),
            _ => continue,
        };
        success = success.msg(1, item);
    }
    Ok(Proto::default().msg(1, success))
}
fn tool_step(call: &ToolCall, result: Option<&ToolResultMessage>) -> Result<Proto> {
    let mut mcp = Proto::default().msg(1, mcp_args(call));
    if let Some(result) = result {
        mcp = mcp.msg(2, mcp_result(result)?);
    }
    Ok(Proto::default().msg(
        2,
        Proto::default()
            .msg(15, mcp)
            .bytes(57, normalized_id(&call.id)),
    ))
}
fn orphan(result: &ToolResultMessage) -> String {
    format!(
        "{}\n{}",
        if result.is_error {
            "[Tool Error]"
        } else {
            "[Tool Result]"
        },
        Content::text_only(&result.content, "\n")
    )
}
fn conversation(context: &Context, model: &Model, blobs: &mut Blobs, end: usize) -> Result<Proto> {
    let system = context
        .system_prompt
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("You are a helpful assistant.");
    let mut state = Proto::default().bytes(
        1,
        store_json(blobs, json!({"role":"system","content":system}))?,
    );
    if end > 0 {
        if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            state = state.bytes(
                1,
                store_json(
                    blobs,
                    json!({"role":"user","content":format!("<rules>\n{system}\n</rules>")}),
                )?,
            );
        }
    }
    let results: HashMap<_, _> = context.messages[..end]
        .iter()
        .filter_map(|m| {
            if let Message::ToolResult(r) = m {
                Some((r.tool_call_id.as_str(), r))
            } else {
                None
            }
        })
        .collect();
    let paired: HashSet<_> = context.messages[..end]
        .iter()
        .filter_map(|m| {
            if let Message::Assistant(a) = m {
                Some(a.content.iter())
            } else {
                None
            }
        })
        .flatten()
        .filter_map(|c| {
            if let Content::ToolCall(t) = c {
                Some(t.id.as_str())
            } else {
                None
            }
        })
        .collect();
    let mut emitted = HashSet::new();
    for message in &context.messages[..end] {
        match message {
            Message::User(user) => {
                state = state.bytes(
                    1,
                    store_json(
                        blobs,
                        json!({"role":"user","content":root_user(&user.content)}),
                    )?,
                )
            }
            Message::Assistant(assistant) => {
                let mut parts = Vec::new();
                let mut issued = HashSet::new();
                for block in &assistant.content {
                    match block {
                Content::Text(t)=>{ if !t.text.is_empty() { parts.push(json!({"type":"text","text":t.text})); } }
                Content::Thinking(t) if model.id.contains("kimi") && assistant.model==model.id && assistant.api==model.api=>parts.push(json!({"type":"reasoning","text":t.thinking,"signature":t.thinking_signature,"providerOptions":{"cursor":{"modelName":model.id}}})),
                Content::ToolCall(call) if issued.insert(&call.id)=>parts.push(json!({"type":"tool-call","toolCallId":normalized_id(&call.id),"toolName":call.name,"args":call.arguments})),
                _=>{}
            }
                }
                if !parts.is_empty() {
                    state = state.bytes(
                        1,
                        store_json(blobs, json!({"role":"assistant","content":parts}))?,
                    );
                }
            }
            Message::ToolResult(result) => {
                if !emitted.insert(&result.tool_call_id) {
                    continue;
                }
                let value = if paired.contains(result.tool_call_id.as_str()) {
                    json!({"role":"tool","id":normalized_id(&result.tool_call_id),"content":[{"type":"tool-result","toolCallId":normalized_id(&result.tool_call_id),"toolName":result.tool_name,"result":Content::text_only(&result.content,"\n"),"isError":result.is_error}]})
                } else {
                    json!({"role":"assistant","content":[{"type":"text","text":orphan(result)}]})
                };
                state = state.bytes(1, store_json(blobs, value)?);
            }
        }
    }
    let mut index = 0;
    while index < end {
        let Message::User(user) = &context.messages[index] else {
            index += 1;
            continue;
        };
        let mut turn = Proto::default().bytes(1, store(blobs, user_message(&user.content)?.0));
        index += 1;
        while index < end && !matches!(&context.messages[index], Message::User(_)) {
            match &context.messages[index] {
                Message::Assistant(a) => {
                    let mut issued = HashSet::new();
                    for block in &a.content {
                        let step = match block {
                            Content::Text(t) if !t.text.is_empty() => {
                                Proto::default().msg(1, Proto::default().bytes(1, &t.text))
                            }
                            Content::Thinking(t)
                                if model.id.contains("kimi")
                                    && a.model == model.id
                                    && a.api == model.api =>
                            {
                                Proto::default().msg(3, Proto::default().bytes(1, &t.thinking))
                            }
                            Content::ToolCall(c) if issued.insert(&c.id) => tool_step(
                                c,
                                results.get(c.id.as_str()).map(|result| result.as_ref()),
                            )?,
                            _ => continue,
                        };
                        turn = turn.bytes(2, store(blobs, step.0));
                    }
                }
                Message::ToolResult(r) if !paired.contains(r.tool_call_id.as_str()) => {
                    turn = turn.bytes(
                        2,
                        store(
                            blobs,
                            Proto::default()
                                .msg(1, Proto::default().bytes(1, orphan(r)))
                                .0,
                        ),
                    )
                }
                _ => {}
            }
            index += 1;
        }
        state = state.bytes(8, store(blobs, Proto::default().msg(1, turn).0));
    }
    Ok(state)
}
pub(super) fn request_context(context: &Context, project_schema: bool) -> Result<Proto> {
    let mut request = Proto::default();
    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        request = request.msg(
            2,
            Proto::default()
                .bytes(1, "/omp/system-prompt/0.mdc")
                .bytes(2, system)
                .msg(3, Proto::default().msg(1, Proto::default()))
                .int(4, 2),
        );
    }
    for tool in &context.tools {
        let schema = serde_json::to_value(&tool.parameters).map_err(|e| e.to_string())?;
        let schema = if project_schema {
            super::schema::project(schema)
        } else {
            schema
        };
        request = request.msg(
            7,
            Proto::default()
                .bytes(1, &tool.name)
                .bytes(2, &tool.description)
                .msg(3, wire::json_encode(&schema))
                .bytes(4, "pi-agent")
                .bytes(5, &tool.name),
        );
    }
    Ok(request)
}
fn route(model: &Model, options: &SimpleStreamOptions) -> (Proto, Proto) {
    let id = wire::request_model_id(model, options);
    let route = setting(model, "cursorModelRoutes").and_then(|m| m.get(id));
    let openai = setting(model, "identity")
        .and_then(|v| v.get("class"))
        .and_then(Value::as_str)
        == Some("openai")
        || id.starts_with("gpt-")
        || id.starts_with("o3")
        || id.starts_with("o4");
    // Legacy discovery gives a usable sibling slug, whereas requested_model
    // wants the base id plus its reasoning parameter. Keep model_details on
    // the usable sibling: the server validates the two independently.
    let legacy = if route.is_none() && openai {
        let (base, fast) = id
            .strip_suffix("-fast")
            .map_or((id, false), |base| (base, true));
        [
            "extra-high",
            "xhigh",
            "minimal",
            "medium",
            "high",
            "low",
            "none",
            "max",
        ]
        .into_iter()
        .find_map(|effort| {
            let base = base.strip_suffix(effort)?.strip_suffix('-')?;
            Some((
                if fast {
                    format!("{base}-fast")
                } else {
                    base.to_owned()
                },
                match effort {
                    "extra-high" => "xhigh",
                    "none" => "off",
                    other => other,
                },
            ))
        })
    } else {
        None
    };
    let route_id = route
        .and_then(|r| r.get("modelId"))
        .and_then(Value::as_str)
        .or_else(|| legacy.as_ref().map(|(base, _)| base.as_str()))
        .unwrap_or(id);
    let max = route
        .and_then(|r| r.get("maxMode"))
        .and_then(Value::as_bool)
        .or_else(|| {
            setting(model, "cursorMaxModeRoutes")
                .and_then(|r| r.get(id))
                .and_then(Value::as_bool)
        })
        .or_else(|| setting(model, "cursorMaxMode").and_then(Value::as_bool))
        .unwrap_or(id.ends_with("-fast") && id.contains("opus"));
    let mut details = Proto::default()
        .bytes(1, id)
        .bytes(3, &model.id)
        .bytes(4, &model.name);
    if max {
        details = details.int(7, 1);
    }
    let mut requested = Proto::default().bytes(1, route_id).int(2, max as u64);
    if let Some(params) = route
        .and_then(|r| r.get("parameters"))
        .or_else(|| setting(model, "cursorModelParameters"))
        .and_then(Value::as_array)
    {
        for parameter in params {
            if let (Some(id), Some(value)) = (parameter["id"].as_str(), parameter["value"].as_str())
            {
                requested = requested.msg(3, Proto::default().bytes(1, id).bytes(2, value));
            }
        }
    } else if let Some((_, effort)) = legacy {
        if effort != "off" {
            requested = requested.msg(3, Proto::default().bytes(1, "reasoning").bytes(2, effort));
        }
    } else if id.starts_with("composer-2.5") && !id.ends_with("-fast") {
        requested = requested.msg(3, Proto::default().bytes(1, "fast").bytes(2, "false"));
    }
    (details, requested)
}
pub(super) fn build(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    blobs: &mut Blobs,
) -> Result<Proto> {
    let active = matches!(context.messages.last(), Some(Message::User(_)));
    let end = context.messages.len() - usize::from(active);
    let state = conversation(context, model, blobs, end)?;
    let action = if let Some(Message::User(user)) = context.messages.last().filter(|_| active) {
        Proto::default().msg(1, Proto::default().msg(1, user_message(&user.content)?))
    } else {
        Proto::default().msg(2, Proto::default())
    };
    let (details, requested) = route(model, options);
    let run = Proto::default()
        .msg(1, state)
        .msg(2, action)
        .msg(3, details)
        .msg(9, requested)
        .bytes(
            5,
            options
                .session_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        );
    Ok(Proto::default().msg(1, run))
}

#[cfg(test)]
mod tests {
    use super::wire::{data, fields, number, text};
    use super::*;
    #[test]
    fn history_and_system_are_content_addressed_not_inline() {
        let model = Model::new(
            "composer-2.5",
            "Composer",
            crate::Api::Other("cursor-agent".into()),
            "cursor",
            "",
        );
        let mut blobs = Blobs::new();
        let body = build(
            &model,
            &Context {
                system_prompt: Some("system".into()),
                ..Default::default()
            },
            &SimpleStreamOptions::default(),
            &mut blobs,
        )
        .unwrap()
        .0;
        let root = fields(&body).unwrap();
        let run = fields(data(&root, 1)).unwrap();
        let state = fields(data(&run, 1)).unwrap();
        let id = data(&state, 1);
        assert_eq!(id.len(), 32);
        let record: Value = serde_json::from_slice(blobs.get(id).unwrap()).unwrap();
        assert_eq!(record["content"], "system");
        let requested = fields(data(&run, 9)).unwrap();
        assert_eq!(text(&requested, 1).unwrap(), "composer-2.5");
        let p = fields(data(&requested, 3)).unwrap();
        assert_eq!(text(&p, 1).unwrap(), "fast");
        assert_eq!(text(&p, 2).unwrap(), "false");
        assert_eq!(number(&requested, 2), 0);
    }
    #[test]
    fn tool_schemas_use_google_protobuf_value() {
        let context = Context {
            tools: vec![crate::Tool {
                name: "lookup".into(),
                description: "Lookup".into(),
                parameters: serde_json::from_value(
                    json!({"type":"object","properties":{"x":{"type":"string"}}}),
                )
                .unwrap(),
                constrained_sampling: None,
            }],
            ..Default::default()
        };
        let bytes = request_context(&context, false).unwrap().0;
        let f = fields(&bytes).unwrap();
        let tool = fields(data(&f, 7)).unwrap();
        assert_eq!(text(&tool, 4).unwrap(), "pi-agent");
        assert_eq!(wire::json_decode(data(&tool, 3)).unwrap()["type"], "object");
    }
    #[test]
    fn reasoning_routes_keep_details_and_requested_identity_distinct() {
        let mut model = Model::new(
            "gpt-5.5-fast",
            "GPT",
            crate::Api::Other("cursor-agent".into()),
            "cursor",
            "",
        );
        model.sampling_params=Some(serde_json::from_value(json!({"requestModelId":"gpt-5.5-none-fast","identity":{"class":"openai"},"thinking":{"effortRouting":{"high":"gpt-5.5-high-fast"}}})).unwrap());
        let options = SimpleStreamOptions {
            reasoning: Some(crate::types::ThinkingLevel::High),
            ..Default::default()
        };
        let (details, requested) = route(&model, &options);
        assert_eq!(
            text(&fields(&details.0).unwrap(), 1).unwrap(),
            "gpt-5.5-high-fast"
        );
        let requested = fields(&requested.0).unwrap();
        assert_eq!(text(&requested, 1).unwrap(), "gpt-5.5-fast");
        let parameter = fields(data(&requested, 3)).unwrap();
        assert_eq!(text(&parameter, 1).unwrap(), "reasoning");
        assert_eq!(text(&parameter, 2).unwrap(), "high");
    }
}
