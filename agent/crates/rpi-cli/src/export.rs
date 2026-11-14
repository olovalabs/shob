//! Session export functionality.
//!
//! Supports multiple export formats: Markdown (current), HTML, and JSONL.

use std::path::Path;

use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::types::{Entry, EntryOrder, EntryQuery};

/// Export format options.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportFormat {
    Markdown,
    Html,
    Jsonl,
}

/// Export a JSONL session file to the specified format.
///
/// The input is a real JSONL v4 session file (header line + one mutation per
/// line), so it is parsed with the harness codec rather than assuming raw
/// `Entry` JSON. Only entry mutations contribute to the rendered transcript;
/// lane/fact/record lines are skipped (they are not conversational content).
pub fn export_file(input: &Path, output: &Path) -> Result<(), String> {
    // Determine format from output extension
    let format = if output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
    {
        ExportFormat::Html
    } else if output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        ExportFormat::Jsonl
    } else {
        ExportFormat::Markdown
    };

    let entries = read_jsonl_session_entries(input)?;

    // `--export` reads a finished session file: every message in it was accepted
    // by storage, so there is nothing to recover here. The recovery copies only
    // exist for a live harness (see `export_session`).
    let unsaved: &[rpi_harness::agent_harness::UnsavedMessage] = &[];

    match format {
        ExportFormat::Markdown => export_markdown(&entries, unsaved, output),
        ExportFormat::Html => export_html(&entries, unsaved, output),
        ExportFormat::Jsonl => export_jsonl(&entries, unsaved, output),
    }
}

/// Parse a JSONL v4 session file into its entry list (oldest first).
fn read_jsonl_session_entries(input: &Path) -> Result<Vec<Entry>, String> {
    use rpi_harness::session::jsonl::parse_mutation;
    use rpi_harness::session::types::SessionMutation;

    let content = std::fs::read_to_string(input)
        .map_err(|e| format!("Could not read input file {}: {e}", input.display()))?;

    let mut entries = Vec::new();
    let mut saw_header = false;
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            // A valid session file starts with the v4 header line. Validate it
            // so a plain JSON/old file fails with a clear message.
            rpi_harness::session::jsonl::codec::parse_header(line).map_err(|e| {
                format!(
                    "{} is not a JSONL v4 session file (line {}): {e}",
                    input.display(),
                    index + 1
                )
            })?;
            saw_header = true;
            continue;
        }
        let mutation = parse_mutation(line).map_err(|e| {
            format!(
                "could not parse mutation at {}:{}: {e}",
                input.display(),
                index + 1
            )
        })?;
        if let SessionMutation::Entry { entry, .. } = mutation {
            entries.push(entry);
        }
    }
    if !saw_header {
        return Err(format!("{} is empty", input.display()));
    }
    Ok(entries)
}

/// Export the session to the specified format.
pub async fn export_session(
    harness: &AgentHarness,
    format: ExportFormat,
    output_path: &Path,
) -> Result<(), String> {
    let tree = harness.session().view("main");
    let entries = tree
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("Could not read session: {e}"))?;

    // A refused append never becomes an entry, so the session view above cannot
    // contain it. Append the harness's recovery copies: the warning tells the user
    // to export before exiting, and this is what makes that promise true.
    let unsaved = harness.unsaved_messages();

    match format {
        ExportFormat::Markdown => export_markdown(&entries, &unsaved, output_path),
        ExportFormat::Html => export_html(&entries, &unsaved, output_path),
        ExportFormat::Jsonl => export_jsonl(&entries, &unsaved, output_path),
    }
}

fn export_markdown(
    entries: &[Entry],
    unsaved: &[rpi_harness::agent_harness::UnsavedMessage],
    output_path: &Path,
) -> Result<(), String> {
    let mut md = String::from("# Session\n\n");
    for e in entries {
        if let Entry::Message(me) = e {
            match &me.message {
                rpi_agent::AgentMessage::User(u) => {
                    let text = user_message_text(u);
                    md.push_str(&format!("## User\n\n{}\n\n", text));
                }
                rpi_agent::AgentMessage::Assistant(a) => {
                    let text = assistant_text(a);
                    if !text.is_empty() {
                        md.push_str(&format!("## Assistant\n\n{}\n\n", text));
                    }
                }
                _ => {}
            }
        }
    }
    // Messages the session never accepted, appended last: they are the most
    // recent turns, and a refused write is exactly the content that exists
    // nowhere else.
    if !unsaved.is_empty() {
        md.push_str("# Not saved\n\n");
        md.push_str(
            "These messages are missing from the session log because saving them was \
             refused. They are reproduced here so this export is the complete \
             conversation.\n\n",
        );
        for failure in unsaved {
            append_unsaved_markdown(&mut md, &failure.message);
        }
    }
    std::fs::write(output_path, md).map_err(|e| format!("Could not write markdown export: {e}"))
}

/// Render one recovery-copied message into a markdown export.
fn append_unsaved_markdown(md: &mut String, message: &rpi_agent::AgentMessage) {
    match message {
        rpi_agent::AgentMessage::User(u) => {
            md.push_str(&format!("## User\n\n{}\n\n", user_message_text(u)));
        }
        rpi_agent::AgentMessage::Assistant(a) => {
            let text = assistant_text(a);
            if !text.is_empty() {
                md.push_str(&format!("## Assistant\n\n{text}\n\n"));
            }
        }
        other => {
            md.push_str(&format!("## Message\n\n```text\n{other:?}\n```\n\n"));
        }
    }
}

fn export_html(
    entries: &[Entry],
    unsaved: &[rpi_harness::agent_harness::UnsavedMessage],
    output_path: &Path,
) -> Result<(), String> {
    let mut html = String::from(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Session Export</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; margin: 2rem; line-height: 1.6; }
        .message { margin-bottom: 2rem; padding: 1rem; border-radius: 8px; }
        .user { background-color: #f0f9ff; border-left: 4px solid #3b82f6; }
        .assistant { background-color: #f3f4f6; border-left: 4px solid #6b7280; }
        .role { font-weight: bold; margin-bottom: 0.5rem; color: #1f2937; }
        pre { background-color: #1e293b; color: #f8fafc; padding: 1rem; border-radius: 6px; overflow-x: auto; }
        code { font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace; }
    </style>
</head>
<body>
    <h1>Session Export</h1>
"#,
    );

    for e in entries {
        if let Entry::Message(me) = e {
            match &me.message {
                rpi_agent::AgentMessage::User(u) => {
                    let text = user_message_text(u);
                    html.push_str(&format!(
                        r#"<div class="message user">
    <div class="role">User</div>
    <div>{}</div>
</div>"#,
                        escape_html(&text)
                    ));
                }
                rpi_agent::AgentMessage::Assistant(a) => {
                    let text = assistant_text(a);
                    if !text.is_empty() {
                        html.push_str(&format!(
                            r#"<div class="message assistant">
    <div class="role">Assistant</div>
    <div>{}</div>
</div>"#,
                            escape_html(&text)
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    if !unsaved.is_empty() {
        html.push_str(
            r#"<h2>Not saved</h2>
<p>These messages are missing from the session log because saving them was refused. They are reproduced here so this export is the complete conversation.</p>
"#,
        );
        for failure in unsaved {
            match &failure.message {
                rpi_agent::AgentMessage::User(u) => {
                    html.push_str(&format!(
                        r#"<div class="message user">
    <div class="role">User</div>
    <div>{}</div>
</div>"#,
                        escape_html(&user_message_text(u))
                    ));
                }
                rpi_agent::AgentMessage::Assistant(a) => {
                    let text = assistant_text(a);
                    if !text.is_empty() {
                        html.push_str(&format!(
                            r#"<div class="message assistant">
    <div class="role">Assistant</div>
    <div>{}</div>
</div>"#,
                            escape_html(&text)
                        ));
                    }
                }
                other => {
                    html.push_str(&format!(
                        r#"<div class="message assistant">
    <div class="role">Message</div>
    <pre>{}</pre>
</div>"#,
                        escape_html(&format!("{other:?}"))
                    ));
                }
            }
        }
    }

    html.push_str("\n</body>\n</html>");

    std::fs::write(output_path, html).map_err(|e| format!("Could not write HTML export: {e}"))
}

fn export_jsonl(
    entries: &[Entry],
    unsaved: &[rpi_harness::agent_harness::UnsavedMessage],
    output_path: &Path,
) -> Result<(), String> {
    let mut jsonl = String::new();
    for e in entries {
        let json = serde_json::to_string(e)
            .map_err(|e| format!("Could not serialize entry to JSON: {e}"))?;
        jsonl.push_str(&json);
        jsonl.push('\n');
    }
    // A JSONL export is a machine-readable stream, so the recovery copies are
    // written as messages with an explicit marker rather than a comment line:
    // the consumer can then tell an unsaved turn from a persisted one.
    for failure in unsaved {
        let json = serde_json::to_string(&serde_json::json!({
            "type": "unsaved_message",
            "reason": failure.what,
            "message": failure.message,
        }))
        .map_err(|e| format!("Could not serialize unsaved message to JSON: {e}"))?;
        jsonl.push_str(&json);
        jsonl.push('\n');
    }
    std::fs::write(output_path, jsonl).map_err(|e| format!("Could not write JSONL export: {e}"))
}

fn user_message_text(user: &rpi_ai::types::UserMessage) -> String {
    match &user.content {
        rpi_ai::types::UserContent::Text(s) => s.clone(),
        rpi_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
                rpi_ai::types::Content::Image(_) => Some("[Image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn assistant_text(assistant: &rpi_ai::types::AssistantMessage) -> String {
    assistant
        .content
        .iter()
        .filter_map(|c| match c {
            rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
            rpi_ai::types::Content::Thinking(t) => {
                Some(format!("<thinking>{}</thinking>", t.thinking))
            }
            rpi_ai::types::Content::ToolCall(tc) => Some(format!("[Tool Call: {}]", tc.name)),
            rpi_ai::types::Content::Image(_) => Some("[Image]".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_harness::session::jsonl::codec::{encode_header, encode_mutation};
    use rpi_harness::session::jsonl::{HeaderKind, JsonlV4Header};
    use rpi_harness::session::types::{Entry, EntryBase, MessageEntry, SessionMutation};

    #[test]
    fn test_escape_html() {
        assert_eq!(
            escape_html("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("Hello & World"), "Hello &amp; World");
    }

    /// Build a real JSONL v4 session file containing one user + one assistant
    /// message, then export it to each format via the `--export` code path.
    #[test]
    fn export_file_renders_a_real_jsonl_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.jsonl");

        let header = JsonlV4Header {
            kind: HeaderKind,
            version: 4,
            id: "s-export".to_string(),
            progress_format: None,
            created_at: 0,
            cwd: "/tmp".to_string(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };

        let mut lines = vec![encode_header(&header)];
        for (seq, message) in [
            (
                1u64,
                rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(
                    rpi_ai::types::UserContent::Text("hello <world>".to_string()),
                    0,
                )),
            ),
            (
                2u64,
                rpi_agent::message::AgentMessage::Assistant(Box::new({
                    let mut a = rpi_ai::types::AssistantMessage::empty(
                        rpi_ai::Api::AnthropicMessages,
                        "anthropic",
                        "m",
                        0,
                    );
                    a.content = vec![rpi_ai::types::Content::Text(rpi_ai::types::TextContent {
                        kind: rpi_ai::types::TextContentType,
                        text: "hi there".to_string(),
                        text_signature: None,
                    })];
                    a
                })),
            ),
        ] {
            let entry = Entry::Message(MessageEntry {
                base: EntryBase {
                    entry_type: "message".to_string(),
                    id: format!("e{seq}"),
                    seq,
                    parent_id: None,
                    timestamp: 0,
                },
                message,
                terminate: None,
            });
            lines.push(encode_mutation(&SessionMutation::Entry {
                seq,
                timestamp: 0,
                lane: Some("main".to_string()),
                entry,
            }));
        }
        std::fs::write(&session_path, lines.concat()).unwrap();

        // Markdown
        let md_path = dir.path().join("out.md");
        export_file(&session_path, &md_path).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        assert!(md.contains("hello <world>"), "{md}");
        assert!(md.contains("hi there"), "{md}");

        // HTML escapes the angle brackets in the user text.
        let html_path = dir.path().join("out.html");
        export_file(&session_path, &html_path).unwrap();
        let html = std::fs::read_to_string(&html_path).unwrap();
        assert!(html.contains("&lt;world&gt;"), "{html}");
        assert!(html.contains("hi there"), "{html}");
        assert!(html.trim_end().ends_with("</html>"), "{html}");

        // JSONL re-emits the parsed entries (not the original wrapper lines).
        let jsonl_path = dir.path().join("out.jsonl");
        export_file(&session_path, &jsonl_path).unwrap();
        let jsonl = std::fs::read_to_string(&jsonl_path).unwrap();
        let parsed: Vec<Entry> = jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.len(), 2);
    }

    /// The write-failure warning tells the user to export before exiting, and
    /// JSONL storage applies an entry only after the append succeeds — so a
    /// refused message exists nowhere the session-backed export can read. This
    /// drives the production export with a recovery copy present and asserts the
    /// file actually contains the refused text, in every format.
    #[test]
    fn a_refused_write_reaches_the_export_through_the_recovery_copy() {
        let dir = tempfile::tempdir().unwrap();
        let message = rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage {
            role: rpi_ai::types::UserRole,
            content: rpi_ai::types::UserContent::Text("only on screen".into()),
            timestamp: 0,
        });
        let unsaved = vec![rpi_harness::agent_harness::UnsavedMessage {
            message,
            what: "a message you sent".to_string(),
            at: 0,
            session_id: "sess".to_string(),
            lane: "main".to_string(),
        }];

        let md_path = dir.path().join("out.md");
        export_markdown(&[], &unsaved, &md_path).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        assert!(md.contains("only on screen"), "{md}");
        assert!(
            md.contains("Not saved"),
            "the export must say the message is not in the session log: {md}"
        );

        let html_path = dir.path().join("out.html");
        export_html(&[], &unsaved, &html_path).unwrap();
        let html = std::fs::read_to_string(&html_path).unwrap();
        assert!(html.contains("only on screen"), "{html}");
        assert!(html.trim_end().ends_with("</html>"), "{html}");

        let jsonl_path = dir.path().join("out.jsonl");
        export_jsonl(&[], &unsaved, &jsonl_path).unwrap();
        let jsonl = std::fs::read_to_string(&jsonl_path).unwrap();
        let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(line["type"], "unsaved_message", "{jsonl}");
        assert_eq!(
            line["message"]["content"], "only on screen",
            "the machine-readable export must carry the message body: {jsonl}"
        );
    }

    #[test]
    fn export_file_rejects_a_non_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("not-a-session.jsonl");
        std::fs::write(&bogus, "{\"type\":\"message\"}\n").unwrap();
        let out = dir.path().join("out.md");
        let err = export_file(&bogus, &out).unwrap_err();
        assert!(err.contains("not a JSONL v4 session file"), "{err}");
    }

    /// The maintainer's isolation probe, verbatim in shape: preload a recovery
    /// copy for session A through the test hook, switch to an empty ephemeral
    /// session B, export B through the production `export_session` — the file
    /// must not contain A's refused content. Combined with the harness-side
    /// `review_session_switch_does_not_expose_previous_session_recovery`, this
    /// pins both ends of the isolation contract (scoped reader + real switch).
    #[tokio::test]
    async fn review_session_switch_does_not_export_previous_session_recovery() {
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        harness.keep_unsaved_message_for_test(
            "old session message",
            rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new("SESSION_A_SECRET", 1)),
        );
        harness
            .set_session(crate::session::ephemeral_session())
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("session-b.md");
        export_session(&harness, ExportFormat::Markdown, &output)
            .await
            .unwrap();
        let text = std::fs::read_to_string(output).unwrap();
        assert!(
            !text.contains("SESSION_A_SECRET"),
            "session B export leaked session A's recovery data: {text}"
        );
    }
}
