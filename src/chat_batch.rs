use embedded::{MAX_BATCH_CALLS, MAX_BATCH_OUTPUT_BYTES};
use gpui::{
    App, Div, MouseDownEvent, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ChatTool, ThemeTokens, ToolState, ToolView, chat_copy_button, chat_output_expander,
    chat_tool_output_block, clip_tool_text, first_line, tool_view,
};

pub fn is_batch(name: &str) -> bool {
    name.eq_ignore_ascii_case("batch")
}

#[derive(Clone, Default)]
pub struct BatchCalls {
    requested: usize,
    views: Vec<ToolView>,
}

pub fn calls(input: Option<&Value>) -> BatchCalls {
    let Some(calls) = input
        .and_then(|input| input.get("calls"))
        .and_then(Value::as_array)
    else {
        return BatchCalls::default();
    };
    let views = calls
        .iter()
        .take(MAX_BATCH_CALLS)
        .map(|call| {
            let name = call.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let mut view = tool_view(name, "", Some(call));
            view.subtitle = clip_tool_text(first_line(&view.subtitle), 160).0;
            view
        })
        .collect();
    BatchCalls {
        requested: calls.len(),
        views,
    }
}

pub fn failure_preview(output: &str) -> Option<String> {
    let diagnostics = output
        .trim_start()
        .trim_start_matches("schema error: ")
        .strip_prefix("Validation failed for tool \"batch\":")?;
    let diagnostic = first_line(diagnostics.trim_start()).trim_start_matches("- ");
    if let Some((_, limit)) = diagnostic
        .strip_prefix("calls: ")
        .and_then(|message| message.rsplit_once(" has more than "))
        && let Some(limit) = limit
            .strip_suffix(" items")
            .and_then(|limit| limit.parse::<usize>().ok())
    {
        return Some(format!(
            "This batch exceeded the {limit}-call limit. Split the calls into smaller batches."
        ));
    }
    Some(format!(
        "Invalid batch arguments. {}",
        clip_tool_text(diagnostic, 240).0
    ))
}

#[derive(Deserialize)]
struct ResultContent {
    #[serde(default)]
    content: Vec<TextBlock>,
}

#[derive(Deserialize)]
struct TextBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct ResultItem {
    index: usize,
    tool: String,
    is_error: bool,
    error: Option<String>,
    result: Option<ResultContent>,
}

pub struct BatchResult {
    items: [Option<BatchItem>; MAX_BATCH_CALLS],
    complete: bool,
}

struct BatchItem {
    title: String,
    failed: bool,
    output: SharedString,
    preview: SharedString,
    omitted_bytes: usize,
}

impl BatchResult {
    pub fn parse(output: &str) -> Option<Self> {
        if output.len() > MAX_BATCH_OUTPUT_BYTES {
            return None;
        }
        let output = output.trim();
        let mut remaining = output.strip_prefix('[')?.trim_start();
        let mut items = [const { None }; MAX_BATCH_CALLS];
        // Older transcripts can end mid-result at the transcript size cap.
        let truncated = output.ends_with("… (truncated)");
        for _ in 0..MAX_BATCH_CALLS {
            let mut stream =
                serde_json::Deserializer::from_str(remaining).into_iter::<ResultItem>();
            let Some(Ok(item)) = stream.next() else {
                break;
            };
            let slot = items.get_mut(item.index)?;
            if slot.is_some() {
                return None;
            }
            let output = item.error.unwrap_or_else(|| {
                item.result
                    .into_iter()
                    .flat_map(|result| result.content)
                    .filter_map(|block| block.text)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            let char_end = output
                .char_indices()
                .nth(500)
                .map_or(output.len(), |(at, _)| at);
            let end = output[..char_end]
                .match_indices('\n')
                .nth(1)
                .map_or(char_end, |(at, _)| at);
            let omitted_bytes = output.len() - end;
            let output = SharedString::from(output);
            let preview = if omitted_bytes > 0 {
                format!("{}…", &output[..end]).into()
            } else {
                output.clone()
            };
            *slot = Some(BatchItem {
                title: crate::tool_title(&item.tool),
                failed: item.is_error,
                output,
                preview,
                omitted_bytes,
            });
            remaining = remaining[stream.byte_offset()..].trim_start();
            if remaining == "]" {
                return Some(Self {
                    items,
                    complete: true,
                });
            }
            let Some(next) = remaining.strip_prefix(',') else {
                break;
            };
            remaining = next.trim_start();
        }
        truncated.then_some(Self {
            items,
            complete: false,
        })
    }

    pub fn failed_count(&self) -> usize {
        self.items
            .iter()
            .flatten()
            .filter(|item| item.failed)
            .count()
    }
}

pub fn call_count_label(count: usize) -> String {
    format!("{count} {}", if count == 1 { "call" } else { "calls" })
}

pub fn known_count(tool: &ChatTool) -> Option<usize> {
    let count = tool.display.batch_calls.requested.max(
        tool.batch_result
            .as_ref()
            .and_then(|result| result.items.iter().rposition(Option::is_some))
            .map_or(0, |last| last + 1),
    );
    (count > 0).then_some(count)
}

pub fn details(
    mut body: Div,
    pane_id: usize,
    message_index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let result = tool.batch_result.as_ref();
    let mut hidden_bytes = 0;
    for index in 0..known_count(tool).unwrap_or(0).min(MAX_BATCH_CALLS) {
        let call = tool.display.batch_calls.views.get(index);
        let item = result.and_then(|result| result.items[index].as_ref());
        let title = call
            .map(|call| call.title.as_str())
            .or_else(|| item.map(|item| item.title.as_str()))
            .unwrap_or("Tool");
        let target = call.map(|call| call.subtitle.as_str()).unwrap_or_default();
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .text_size(px(12.0))
            .text_color(tokens.text_secondary)
            .child(div().flex_shrink_0().child(title.to_owned()))
            .when(!target.is_empty(), |row| {
                row.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(tokens.text_muted)
                        .child(target.to_owned()),
                )
            })
            .child(div().flex_1());
        if let Some(item) = item {
            hidden_bytes += item.omitted_bytes;
            if item.failed {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .text_color(tokens.block_failure)
                        .child("Failed"),
                );
            }
            if !item.output.is_empty() {
                row = row.child(chat_copy_button(
                    format!("batch-copy-{pane_id}-{message_index}-{index}").into(),
                    item.output.clone(),
                    "Copy output".into(),
                    tokens,
                ));
            }
        }
        body = body.child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(row)
                .when_some(item.filter(|item| !item.preview.is_empty()), |row, item| {
                    row.child(chat_tool_output_block(
                        if tool.show_full {
                            item.output.clone()
                        } else {
                            item.preview.clone()
                        },
                        item.failed,
                        tokens,
                    ))
                }),
        );
    }
    if hidden_bytes > 0 {
        body = body.child(chat_output_expander(
            format!("batch-more-{pane_id}-{message_index}").into(),
            hidden_bytes,
            tool.show_full,
            tokens,
            on_toggle,
        ));
    }
    let notice = match result {
        Some(result) if !result.complete => {
            Some("Some results were truncated in the saved conversation.")
        }
        None if tool.state == ToolState::Unknown => Some("Execution ended without a result."),
        _ => None,
    };
    body.when_some(notice, |body, notice| {
        body.child(
            div()
                .text_size(px(12.0))
                .text_color(tokens.text_muted)
                .child(notice),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_validation_preview_explains_the_limit_without_echoing_arguments() {
        let output = "schema error: Validation failed for tool \"batch\":\n  - calls: [{\"tool\":\"read\"}] has more than 4 items\n\nReceived arguments:\n{}";
        assert_eq!(
            failure_preview(output).as_deref(),
            Some("This batch exceeded the 4-call limit. Split the calls into smaller batches.")
        );
    }

    #[test]
    fn batch_results_decode_text_and_keep_failures_at_their_call_index() {
        let first = r#"{"index":0,"tool":"find","is_error":false,"result":{"content":[{"type":"text","text":"a.rs\nb.rs"}],"details":{"performance":{}}}}"#;
        let second = r#"{"index":4,"tool":"read","is_error":true,"error":"File not found"}"#;
        let parsed = BatchResult::parse(&format!("[{first},{second}]")).unwrap();
        assert!(parsed.complete);
        assert_eq!(parsed.failed_count(), 1);
        assert_eq!(
            parsed.items[0].as_ref().unwrap().output.as_ref(),
            "a.rs\nb.rs"
        );
        assert_eq!(
            parsed.items[4].as_ref().unwrap().output.as_ref(),
            "File not found"
        );

        let restored = BatchResult::parse(&format!("[{first},{{… (truncated)")).unwrap();
        assert!(!restored.complete);
        assert!(restored.items[0].is_some());
        assert!(restored.items[1].is_none());
        assert!(BatchResult::parse("batch requires 1–4 calls").is_none());
    }
}
