use crate::{Content, Context, Message, Model, SimpleStreamOptions, ThinkingLevel, UserContent};
use serde_json::{json, Value};

fn image(image: &crate::ImageContent) -> Result<Value, String> {
    let format = match image.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unsupported Bedrock image type: {other}")),
    };
    Ok(json!({"image":{"format":format,"source":{"bytes":image.data}}}))
}
fn id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
fn blocks(content: &[Content], assistant: bool, native: bool) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    for block in content {
        match block {
            Content::Text(t) if !t.text.trim().is_empty() => result.push(json!({"text":t.text})),
            Content::Image(i) if !assistant => result.push(image(i)?),
            Content::ToolCall(t) if assistant => result
                .push(json!({"toolUse":{"toolUseId":id(&t.id),"name":t.name,"input":t.arguments}})),
            Content::Thinking(t) if assistant && native && t.thinking_signature.is_some() => {
                if t.redacted {
                    result
                        .push(json!({"reasoningContent":{"redactedContent":t.thinking_signature}}));
                } else {
                    result.push(json!({"reasoningContent":{"reasoningText":{"text":t.thinking,"signature":t.thinking_signature}}}));
                }
            }
            Content::Thinking(t) if assistant && !t.thinking.trim().is_empty() => {
                result.push(json!({"text":t.thinking}))
            }
            _ => {}
        }
    }
    Ok(result)
}
pub(super) fn build(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < ctx.messages.len() {
        let (role, content) = match &ctx.messages[i] {
            Message::User(user) => (
                "user",
                match &user.content {
                    UserContent::Text(text) => {
                        if text.trim().is_empty() {
                            Vec::new()
                        } else {
                            vec![json!({"text":text})]
                        }
                    }
                    UserContent::Blocks(content) => blocks(content, false, false)?,
                },
            ),
            Message::Assistant(assistant) => (
                "assistant",
                blocks(
                    &assistant.content,
                    true,
                    assistant.provider == model.provider
                        && assistant.model == model.id
                        && assistant.api == model.api,
                )?,
            ),
            Message::ToolResult(_) => {
                let mut content = Vec::new();
                let mut hoisted = Vec::new();
                while let Some(Message::ToolResult(result)) = ctx.messages.get(i) {
                    let mut output = Vec::new();
                    for block in &result.content {
                        match block {
                            Content::Image(img) if result.is_error => {
                                output.push(json!({"text":"(see attached image)"}));
                                hoisted.push(image(img)?);
                            }
                            Content::Image(img) => output.push(image(img)?),
                            Content::Text(t) => output.push(json!({"text":t.text})),
                            _ => {}
                        }
                    }
                    if output.is_empty() {
                        output.push(json!({"text":"(empty tool result)"}));
                    }
                    content.push(json!({"toolResult":{"toolUseId":id(&result.tool_call_id),"content":output,"status":if result.is_error { "error" } else { "success" }}}));
                    i += 1;
                }
                i -= 1;
                content.extend(hoisted);
                ("user", content)
            }
        };
        if !content.is_empty() {
            messages.push(json!({"role":role,"content":content}));
        }
        i += 1;
    }
    let mut inference = serde_json::Map::new();
    if let Some(max) = opts
        .max_tokens
        .or((model.max_tokens > 0).then_some(model.max_tokens))
    {
        inference.insert("maxTokens".into(), json!(max));
    }
    let thinking = model.reasoning && opts.reasoning.is_some_and(|r| r != ThinkingLevel::Off);
    if !thinking {
        if let Some(temperature) = opts.temperature {
            inference.insert("temperature".into(), json!(temperature));
        }
    }
    let mut body = json!({"messages":messages,"inferenceConfig":inference});
    if let Some(prompt) = ctx.system_prompt.as_ref().filter(|p| !p.is_empty()) {
        body["system"] = json!([{"text":prompt}]);
    }
    if !ctx.tools.is_empty() {
        body["toolConfig"] = json!({"tools":ctx.tools.iter().map(|t| json!({"toolSpec":{"name":t.name,"description":t.description,"inputSchema":{"json":t.parameters.0}}})).collect::<Vec<_>>()});
    }
    // Catalog sampling parameters can carry Converse-native model extensions.
    if let Some(params) = &model.sampling_params {
        for (key, value) in params {
            if matches!(
                key.as_str(),
                "additionalModelRequestFields"
                    | "guardrailConfig"
                    | "performanceConfig"
                    | "serviceTier"
                    | "requestMetadata"
                    | "additionalModelResponseFieldPaths"
            ) {
                body[key] = value.clone();
            } else if matches!(key.as_str(), "topP" | "stopSequences") {
                body["inferenceConfig"][key] = value.clone();
            }
        }
    }
    if thinking && body.get("additionalModelRequestFields").is_none() {
        let level = opts.reasoning.unwrap();
        let effort = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&level))
            .and_then(Option::as_ref)
            .map(String::as_str)
            .unwrap_or(match level {
                ThinkingLevel::Minimal => "minimal",
                ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::Xhigh => "xhigh",
                _ => "high",
            });
        if model.id.contains("openai.") {
            body["additionalModelRequestFields"] = json!({"reasoning":{"effort":effort}});
        } else if model.id.contains("anthropic.") {
            let adaptive = ["opus-4-6", "sonnet-4-6", "opus-4-7", "mythos", "fable"]
                .iter()
                .any(|part| model.id.contains(part));
            if adaptive {
                body["additionalModelRequestFields"] =
                    json!({"thinking":{"type":"adaptive"},"output_config":{"effort":effort}});
            } else {
                let budgets = opts.thinking_budgets.unwrap_or_default();
                let budget = match level {
                    ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
                    ThinkingLevel::Low => budgets.low.unwrap_or(2048),
                    ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
                    ThinkingLevel::High => budgets.high.unwrap_or(16384),
                    _ => budgets.xhigh.unwrap_or(32768),
                };
                let max = body["inferenceConfig"]["maxTokens"]
                    .as_u64()
                    .unwrap_or(32768)
                    .max(budget + 1024);
                body["inferenceConfig"]["maxTokens"] = json!(max);
                body["additionalModelRequestFields"] =
                    json!({"thinking":{"type":"enabled","budget_tokens":budget}});
            }
        }
    }
    if let Some(metadata) = &opts.metadata {
        let clean: serde_json::Map<String, Value> = metadata
            .iter()
            .filter(|(k, v)| {
                !k.is_empty()
                    && k.len() <= 256
                    && !v.is_empty()
                    && v.len() <= 256
                    && k.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            })
            .take(16)
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        if !clean.is_empty() {
            body["requestMetadata"] = Value::Object(clean);
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_signed_thinking_only_for_native_history() {
        let content = vec![Content::Thinking(crate::ThinkingContent {
            kind: Default::default(),
            thinking: "reason".into(),
            thinking_signature: Some("sig".into()),
            redacted: false,
        })];
        assert_eq!(
            blocks(&content, true, true).unwrap()[0]["reasoningContent"]["reasoningText"]
                ["signature"],
            "sig"
        );
        assert_eq!(
            blocks(&content, true, false).unwrap()[0],
            json!({"text":"reason"})
        );
    }
    #[test]
    fn encodes_tool_definitions_in_converse_shape() {
        let model = Model::new(
            "a",
            "a",
            crate::Api::BedrockConverseStream,
            "amazon-bedrock",
            "https://example.com",
        );
        let mut ctx = Context::default();
        ctx.tools.push(crate::Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}).into(),
            constrained_sampling: None,
        });
        let body = build(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        assert!(body.get("stream").is_none());
    }
}
