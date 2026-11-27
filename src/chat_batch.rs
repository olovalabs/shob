use embedded::{MAX_BATCH_CALLS, MAX_BATCH_OUTPUT_BYTES};
use gpui::{
    CursorStyle, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, SharedString,
    Styled, div, prelude::FluentBuilder, px, svg,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ChatTool, ThemeTokens, ToolState, UiHost, chat_copy_button, chat_output_expander,
    chat_tool_output_block, clip_tool_text, first_line, tool_view,
};

pub fn is_batch(name: &str) -> bool {
    name.eq_ignore_ascii_case("batch")
}

#[derive(Clone, Default)]
pub struct BatchCalls {
    requested: usize,
    views: Vec<Call>,
    summary: SharedString,
}

#[derive(Clone)]
struct Call {
    title: SharedString,
    subtitle: SharedString,
}

pub fn calls(input: Option<&Value>) -> BatchCalls {
    let Some(calls) = input
        .and_then(|input| input.get("calls"))
        .and_then(Value::as_array)
    else {
        return BatchCalls::default();
    };
    let views: Vec<Call> = calls
        .iter()
        .take(MAX_BATCH_CALLS)
        .map(|call| {
            let name = call.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let mut view = tool_view(name, "", Some(call));
            for arg in &view.args {
                if !view.subtitle.is_empty() {
                    view.subtitle.push_str(" · ");
                }
                view.subtitle.push_str(arg);
            }
            Call {
                title: view.title.into(),
                subtitle: clip_tool_text(first_line(&view.subtitle), 160).0.into(),
            }
        })
        .collect();
    BatchCalls {
        summary: counted_titles(views.iter().map(|call| call.title.as_str())),
        requested: calls.len(),
        views,
    }
}

pub fn failure_preview(output: &str) -> Option<String> {
    let output = output.trim_start().trim_start_matches("schema error: ");
    let diagnostics = output
        .strip_prefix("Invalid arguments for \"batch\".")
        .or_else(|| output.strip_prefix("Validation failed for tool \"batch\":"))?;
    let diagnostic = first_line(diagnostics.trim_start()).trim_start_matches("- ");
    if let Some(limit) = diagnostic
        .strip_prefix("\"calls\": has more than ")
        .or_else(|| {
            diagnostic
                .strip_prefix("calls: ")
                .and_then(|message| message.rsplit_once(" has more than "))
                .map(|(_, limit)| limit)
        })
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
    summary: SharedString,
    failed: usize,
}

struct BatchItem {
    title: SharedString,
    failed: bool,
    output: SharedString,
    preview: SharedString,
    omitted_bytes: usize,
}

impl BatchResult {
    fn new(items: [Option<BatchItem>; MAX_BATCH_CALLS], complete: bool) -> Self {
        Self {
            summary: counted_titles(items.iter().flatten().map(|item| item.title.as_str())),
            failed: items.iter().flatten().filter(|item| item.failed).count(),
            items,
            complete,
        }
    }

    pub fn failed_count(&self) -> usize {
        self.failed
    }

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
                title: crate::tool_title(&item.tool).into(),
                failed: item.is_error,
                output,
                preview,
                omitted_bytes,
            });
            remaining = remaining[stream.byte_offset()..].trim_start();
            if remaining == "]" {
                return Some(Self::new(items, true));
            }
            let Some(next) = remaining.strip_prefix(',') else {
                break;
            };
            remaining = next.trim_start();
        }
        truncated.then(|| Self::new(items, false))
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

fn counted_titles<'a>(titles: impl Iterator<Item = &'a str>) -> SharedString {
    use std::fmt::Write;
    let mut groups = [("", 0usize); MAX_BATCH_CALLS];
    let mut count = 0;
    for title in titles.take(MAX_BATCH_CALLS) {
        if let Some((_, total)) = groups[..count].iter_mut().find(|(name, _)| *name == title) {
            *total += 1;
        } else {
            groups[count] = (title, 1);
            count += 1;
        }
    }
    let mut summary = String::new();
    for (index, (name, total)) in groups[..count].iter().enumerate() {
        if index > 0 {
            summary.push_str(" · ");
        }
        let _ = write!(summary, "{name} {total}");
    }
    summary.into()
}

pub fn counts(tool: &ChatTool) -> SharedString {
    if !tool.display.batch_calls.summary.is_empty() {
        tool.display.batch_calls.summary.clone()
    } else {
        tool.batch_result
            .as_ref()
            .map_or_else(SharedString::default, |result| result.summary.clone())
    }
}

pub fn title(tool: &ChatTool) -> String {
    let mut titles = [""; MAX_BATCH_CALLS];
    let mut count = 0;
    for index in 0..known_count(tool).unwrap_or(0).min(MAX_BATCH_CALLS) {
        let (title, _) = call_label(tool, index);
        if !titles[..count].contains(&title) {
            titles[count] = title;
            count += 1;
        }
    }
    if count == 0 {
        "Tools".into()
    } else {
        titles[..count].join(", ")
    }
}

pub fn activity_details(tool: &ChatTool) -> String {
    (0..known_count(tool).unwrap_or(0).min(MAX_BATCH_CALLS))
        .map(|index| {
            let (title, target) = call_label(tool, index);
            if target.is_empty() {
                title.to_owned()
            } else {
                format!("{title} · {target}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn call_label(tool: &ChatTool, index: usize) -> (&str, &str) {
    let call = tool.display.batch_calls.views.get(index);
    let item = tool
        .batch_result
        .as_ref()
        .and_then(|result| result.items[index].as_ref());
    (
        call.map(|call| call.title.as_str())
            .or_else(|| item.map(|item| item.title.as_str()))
            .unwrap_or("Tool"),
        call.map(|call| call.subtitle.as_str()).unwrap_or_default(),
    )
}

pub fn details(
    mut body: Div,
    pane_id: usize,
    message_index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    host: &UiHost,
) -> Div {
    let result = tool.batch_result.as_ref();
    for index in 0..known_count(tool).unwrap_or(0).min(MAX_BATCH_CALLS) {
        let call = tool.display.batch_calls.views.get(index);
        let item = result.and_then(|result| result.items[index].as_ref());
        let title = call
            .map(|call| call.title.clone())
            .or_else(|| item.map(|item| item.title.clone()))
            .unwrap_or_else(|| "Tool".into());
        let expanded = tool.expanded_calls[index];
        let failed = item.is_some_and(|item| item.failed);
        let status = match item {
            Some(item) if item.failed => Some("Failed"),
            Some(_) => None,
            None if tool.state == ToolState::Running => Some("Running…"),
            None if tool.state == ToolState::Failed => Some("Not run"),
            None => Some("No result"),
        };
        let header = div()
            .id(SharedString::from(format!(
                "tool-call-{pane_id}-{message_index}-{index}"
            )))
            .debug_selector(move || format!("tool-call-{message_index}-{index}"))
            .w_full()
            .min_w_0()
            .h(px(28.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.0))
            .text_color(if failed {
                tokens.block_failure
            } else {
                tokens.text_primary
            })
            .cursor(CursorStyle::PointingHand)
            .hover(move |style| style.bg(tokens.card_hover))
            .on_mouse_down(
                MouseButton::Left,
                host.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_chat_batch_call(pane_id, message_index, index, cx);
                }),
            )
            .child(
                svg()
                    .path(if expanded {
                        "icons/chevron-down.svg"
                    } else {
                        "icons/chevron-right.svg"
                    })
                    .size(px(12.0))
                    .flex_shrink_0()
                    .text_color(tokens.text_muted),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .when_some(
                call.filter(|call| !call.subtitle.is_empty()),
                |row, call| {
                    row.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(tokens.text_muted)
                            .child(call.subtitle.clone()),
                    )
                },
            )
            .child(div().flex_1())
            .when_some(status, |row, status| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_color(if failed {
                            tokens.block_failure
                        } else {
                            tokens.text_muted
                        })
                        .child(status),
                )
            })
            .when_some(
                item.filter(|item| expanded && !item.output.is_empty()),
                |row, item| {
                    row.child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(chat_copy_button(
                                format!("call-copy-{pane_id}-{message_index}-{index}").into(),
                                item.output.clone(),
                                "Copy output".into(),
                                tokens,
                            )),
                    )
                },
            );
        let mut row = div().w_full().min_w_0().flex().flex_col().child(header);
        if expanded {
            if let Some(item) = item.filter(|item| !item.output.is_empty()) {
                row = row.child(
                    div()
                        .pl(px(20.0))
                        .py(px(6.0))
                        .debug_selector(move || format!("tool-call-output-{message_index}-{index}"))
                        .child(chat_tool_output_block(
                            format!("call-output-{pane_id}-{message_index}-{index}").into(),
                            if tool.show_full {
                                item.output.clone()
                            } else {
                                item.preview.clone()
                            },
                            item.failed,
                            tokens,
                        ))
                        .when(item.omitted_bytes > 0, |body| {
                            body.child(chat_output_expander(
                                format!("call-more-{pane_id}-{message_index}-{index}").into(),
                                item.omitted_bytes,
                                tool.show_full,
                                tokens,
                                host.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_chat_tool_show_full(pane_id, message_index, cx);
                                }),
                            ))
                        }),
                );
            } else if item.is_some() {
                row = row.child(
                    div()
                        .pl(px(20.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .text_color(tokens.text_muted)
                        .child("No output"),
                );
            }
        }
        body = body.child(row);
    }
    if result.is_none() && tool.state != ToolState::Running && !tool.output.is_empty() {
        body = body
            .child(div().flex().justify_end().child(chat_copy_button(
                format!("calls-error-copy-{pane_id}-{message_index}").into(),
                tool.output.clone(),
                "Copy output".into(),
                tokens,
            )))
            .child(chat_tool_output_block(
                format!("calls-error-{pane_id}-{message_index}").into(),
                if tool.show_full {
                    tool.output.clone()
                } else {
                    tool.preview.clone()
                },
                tool.state == ToolState::Failed,
                tokens,
            ))
            .when(tool.preview_truncated, |body| {
                body.child(chat_output_expander(
                    format!("calls-error-more-{pane_id}-{message_index}").into(),
                    tool.output.len().saturating_sub(tool.preview.len()),
                    tool.show_full,
                    tokens,
                    host.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_chat_tool_show_full(pane_id, message_index, cx);
                    }),
                ))
            });
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
        for output in [
            "schema error: Validation failed for tool \"batch\":\n  - calls: [{\"tool\":\"read\"}] has more than 4 items\n\nReceived arguments:\n{}",
            "schema error: Invalid arguments for \"batch\".\n- \"calls\": has more than 4 items\nCorrect these fields using the advertised tool parameters, then call the tool again.",
        ] {
            assert_eq!(
                failure_preview(output).as_deref(),
                Some("This batch exceeded the 4-call limit. Split the calls into smaller batches.")
            );
        }
    }

    #[test]
    fn batch_results_decode_text_and_keep_failures_at_their_call_index() {
        let first = r#"{"index":0,"tool":"find","is_error":false,"result":{"content":[{"type":"text","text":"a.rs\nb.rs"}],"details":{"performance":{}}}}"#;
        let second = r#"{"index":4,"tool":"read","is_error":true,"error":"File not found"}"#;
        let parsed = BatchResult::parse(&format!("[{first},{second}]")).unwrap();
        assert!(parsed.complete);
        assert_eq!(parsed.summary.as_str(), "Glob 1 · Read 1");
        assert_eq!(parsed.failed_count(), 1);
        let input = serde_json::json!({"calls": [
            {"tool": "read"}, {"tool": "write"}, {"tool": "bash"},
            {"tool": "read_file"}, {"tool": "shell"}
        ]});
        assert_eq!(
            calls(Some(&input)).summary.as_str(),
            "Read 2 · Write 1 · Shell 2"
        );
        assert!(parsed.items[4].as_ref().unwrap().failed);
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
