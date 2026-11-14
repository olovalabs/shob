//! M5d integration — single-call compaction summary via faux. Mirrors the
//! `compact`/`generateSummaryWithUsage` cases in
//! `packages/agent/test/harness/compaction.test.ts`:
//! - a scripted faux summary reply is consumed by `compact` on a non-split-turn
//!   preparation;
//! - the returned `CompactResult.summary` carries the faux reply text + the
//!   appended file-operations tags;
//! - error / `aborted` stop reasons map to `CompactionError`.
//!
//! Runs against the public `rpi_harness::compaction::{prepare_compaction, compact,
//! CompactionError, CompactionLlmOptions}` surface + `rpi_ai`'s faux provider.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::{StopReason, UserContent, UserMessage};
use rpi_ai::{Model, Provider};
use rpi_harness::compaction::{compact, prepare_compaction, CompactResult, CompactionLlmOptions};
use rpi_harness::session::types::{Entry, EntryBase, MessageEntry};
use rpi_harness::types::{CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
use tokio_util::sync::CancellationToken;

// ---- entry builders (mirror the TS createMessageEntry helpers) ----

fn base(seq: u64, parent: Option<&str>) -> EntryBase {
    EntryBase {
        entry_type: "message".to_string(),
        id: format!("entry-{seq}"),
        seq,
        parent_id: parent.map(|s| s.to_string()),
        timestamp: seq as i64,
    }
}

fn user_msg(text: &str, seq: u64, parent: Option<&str>) -> Entry {
    Entry::Message(MessageEntry {
        base: base(seq, parent),
        message: AgentMessage::User(UserMessage::new(
            UserContent::Text(text.to_string()),
            seq as i64,
        )),
        terminate: None,
    })
}

fn assistant_msg(text: &str, seq: u64, parent: Option<&str>) -> Entry {
    let mut m = rpi_ai::types::AssistantMessage::empty(
        rpi_ai::types::Api::AnthropicMessages,
        "anthropic",
        "claude-sonnet-4-5",
        seq as i64,
    );
    m.content = vec![rpi_ai::types::Content::text(text)];
    m.stop_reason = StopReason::Stop;
    Entry::Message(MessageEntry {
        base: base(seq, parent),
        message: AgentMessage::Assistant(Box::new(m)),
        terminate: None,
    })
}

fn faux_provider_model() -> (Arc<FauxProvider>, Model) {
    let script = FauxScript::new();
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    (provider, model)
}

fn llm_options(provider: Arc<dyn Provider>, model: Model) -> CompactionLlmOptions {
    CompactionLlmOptions {
        provider,
        model,
        api_key: None,
        signal: CancellationToken::new(),
        thinking_level: None,
        retry: None,
        custom_instructions: None,
        stream_options: Default::default(),
    }
}

#[tokio::test]
async fn compact_single_call_returns_summary_and_file_ops() {
    // A long-ish history so prepare_compaction summarizes some of it.
    let mut entries: Vec<Entry> = Vec::new();
    let mut parent: Option<&str> = None;
    let mut parent_owned = String::new();
    for i in 0..6 {
        let u = user_msg(&format!("User {i}"), (2 * i + 1) as u64, parent);
        let u_id = format!("entry-{}", 2 * i + 1);
        let a = assistant_msg(&format!("Assistant {i}"), (2 * i + 2) as u64, Some(&u_id));
        entries.push(u);
        entries.push(a);
        parent_owned = format!("entry-{}", 2 * i + 2);
        parent = Some(parent_owned.as_str());
    }
    let _ = parent_owned;

    // Small keep_recent so prepare_compaction actually yields messages to summarize.
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 2000,
        keep_recent_tokens: 20,
    };
    let preparation = prepare_compaction(&entries, settings).unwrap().unwrap();
    assert!(!preparation.is_split_turn);

    let (provider, model) = faux_provider_model();
    provider
        .script()
        .set_responses(vec![FauxStep::text("## Goal\nTest summary")]);

    let result: CompactResult = compact(
        &preparation,
        &llm_options(provider.clone() as Arc<dyn Provider>, model),
    )
    .await
    .expect("compact should succeed");

    assert!(
        result.summary.contains("Test summary"),
        "summary was: {}",
        result.summary
    );
    // No file ops in the fixture → file-ops section must be empty.
    assert!(!result.summary.contains("<read-files>"));
    assert!(
        result.usage.is_some(),
        "single-call compaction carries the call usage"
    );
    assert!(result.details.is_some());
}

#[tokio::test]
async fn compact_maps_error_stop_reason_to_summarization_failed() {
    // A minimal preparation with one message to summarize + a split turn is NOT
    // required; a single history call that errors yields summarization_failed.
    let preparation = rpi_harness::compaction::CompactionPreparation {
        messages_to_summarize: vec![AgentMessage::User(UserMessage::new(
            UserContent::Text("Summarize this.".to_string()),
            0,
        ))],
        turn_prefix_messages: Vec::new(),
        retained_tail: Vec::new(),
        is_split_turn: false,
        tokens_before: 100,
        previous_summary: None,
        file_ops: rpi_harness::compaction::FileOperations::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };

    let (provider, model) = faux_provider_model();
    // Faux step carrying an error terminal message.
    let mut err =
        rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 0);
    err.stop_reason = StopReason::Error;
    err.error_message = Some("boom".to_string());
    provider
        .script()
        .set_responses(vec![FauxStep::Message(err)]);

    let err = compact(
        &preparation,
        &llm_options(provider.clone() as Arc<dyn Provider>, model),
    )
    .await
    .expect_err("error stop reason must map to a CompactionError");
    assert_eq!(err.code.as_str(), "summarization_failed");
    assert!(err.message.contains("boom"), "message was: {}", err.message);
}

#[tokio::test]
async fn prepare_compaction_noop_on_empty_and_on_last_compaction() {
    assert!(prepare_compaction(&[], DEFAULT_COMPACTION_SETTINGS)
        .unwrap()
        .is_none());

    let compaction = Entry::Compaction(rpi_harness::session::types::CompactionEntry {
        base: base_of("compaction", "c1", 1, None),
        summary: "already".to_string(),
        retained_tail: Vec::new(),
        tokens_before: 0,
        details: None,
        usage: None,
    });
    assert!(
        prepare_compaction(&[compaction], DEFAULT_COMPACTION_SETTINGS)
            .unwrap()
            .is_none()
    );
}

fn base_of(entry_type: &str, id: &str, seq: u64, parent: Option<&str>) -> EntryBase {
    EntryBase {
        entry_type: entry_type.to_string(),
        id: id.to_string(),
        seq,
        parent_id: parent.map(|s| s.to_string()),
        timestamp: seq as i64,
    }
}
