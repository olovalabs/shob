//! Token-efficiency layer: prompt-cache hygiene, a tool-result cache, anchor
//! edits, output minimization, cheap-task routing, and per-turn measurement.
//!
//! Design and reference analysis live in `src/token_efficiency/README.md`.
//!
//! # Shape
//!
//! The decisions live in pure functions over plain data ([`hooks`], [`cache`],
//! [`anchors`], [`minimize`]) so they can be tested without building a harness.
//! This module is the only place that names rpi types, and it is deliberately
//! thin: it maps `AgentToolResult` → text → the pure decision →
//! `AfterToolCallResult`.
//!
//! # Wiring
//!
//! `AgentLoopConfig` already runs both hooks
//! (`agent/crates/agent/src/agent_loop.rs`). `before_tool_call` already reached
//! the harness through `SessionOptions`; this work added the matching
//! `after_tool_call` wire, because the loop supported it but embedded hosts could
//! not reach it. The upstream diff is three small pieces:
//!
//! - `SessionOptions::after_tool_call` + `with_after_tool_call`
//!   (`agent/crates/embedded/src/lib.rs`)
//! - `cli::session::build_with_tool_hooks`, threaded into
//!   `AgentHarnessOptions::after_tool_call` (`agent/crates/cli/src/session.rs`)
//! - `SessionTitleRequest::spawn_routed` for cheap-model title generation
//!
//! Test seams ([`fsprobe::testing::FakeFs`]) keep the tests hermetic and let the
//! replay benchmark drive the real cache without touching the disk; production
//! uses [`fsprobe::OsFileProbe`].

#![allow(dead_code)]

pub mod anchors;
pub mod bench;
pub mod bench_main;
pub mod cache;
pub mod config;
pub mod fsprobe;
pub mod hash;
pub mod hooks;
pub mod metrics;
pub mod minimize;
pub mod prefix;
pub mod routing;

pub use config::TokenEfficiencySettings;
pub use hooks::{After, Before, TokenEfficiency};
pub use metrics::{MetricsSink, Summary, TurnRow};
#[allow(unused_imports)]
pub use prefix::{PrefixGuard, PrefixSnapshot};
pub use routing::{Role, Router};

use std::sync::Arc;

use embedded::{AfterToolCallResult, BeforeToolCallResult, TextContentOrImage};

/// Build the `after_tool_call` hook for one conversation.
///
/// This is what `AgentHarnessOptions::after_tool_call` takes. It does no I/O
/// beyond a stat/hash of the file a tool just touched, and the harness calls it
/// after the tool's own I/O has completed, off the GPUI thread.
pub fn after_tool_call(layer: Arc<TokenEfficiency>) -> embedded::AfterToolCall {
    Arc::new(move |context, _cancel| {
        let layer = Arc::clone(&layer);
        let tool = context.tool_call.name.clone();
        let args = context.args.clone();
        let is_error = context.is_error;
        let text = join_text(&context.result.content);
        Box::pin(async move {
            if is_error {
                return None;
            }
            // Write-invalidation must happen even when the result itself is
            // left alone, so it runs on both branches.
            hooks::note_write(&layer, &tool, &args);
            match hooks::observe_result(&layer, &tool, &args, &text, is_error) {
                After::Continue => None,
                After::Replace(replacement) => Some(AfterToolCallResult {
                    content: Some(vec![TextContentOrImage::text(replacement)]),
                    ..AfterToolCallResult::default()
                }),
            }
        })
    })
}

/// Whether this layer changed the arguments.
enum Args {
    Original,
    Rewritten(serde_json::Value),
}

/// Compose this layer's `before_tool_call` in front of an existing hook.
///
/// Order matters and is deliberate:
///
/// 1. This layer runs first, so an anchor edit is expanded *before* the approval
///    gate sees it. The gate is then judging the `oldText`/`newText` that will
///    actually be written, not an anchor it cannot display.
/// 2. A cache short-circuit returns without consulting the gate: no tool runs,
///    so there is nothing to approve.
/// 3. The gate still runs on every path that executes a tool, and its veto wins.
pub fn compose_before_hook(
    layer: Arc<TokenEfficiency>,
    existing: Option<embedded::BeforeToolCall>,
) -> embedded::BeforeToolCall {
    Arc::new(move |call, cancelled| {
        // Copy everything the decision needs out of `call`: the returned future
        // must be `'static`, so no borrow of the context can cross the await.
        let tool = call.tool_call.name.clone();
        let original = call.args.clone();

        match hooks::intercept(&layer, &tool, &original) {
            Before::ShortCircuit(message) => {
                return Box::pin(async move {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(message),
                        terminate: false,
                        args: None,
                    })
                });
            }
            Before::Continue => {}
            Before::Rewrite(rewritten) => {
                // The gate must judge the arguments that will actually be used.
                let Some(gate) = existing.clone() else {
                    return Box::pin(async move {
                        Some(BeforeToolCallResult {
                            block: false,
                            reason: None,
                            terminate: false,
                            args: Some(rewritten),
                        })
                    });
                };
                let arguments = rewritten.clone();
                let rebuilt = embedded::BeforeToolCallContext {
                    assistant_message: call.assistant_message,
                    tool_call: call.tool_call,
                    args: &arguments,
                    context: call.context,
                };
                return gate(rebuilt, cancelled);
            }
        }

        match &existing {
            Some(gate) => gate(call, cancelled),
            None => Box::pin(async { None }),
        }
    })
}

/// The layer's settings as configured in `settings.json`, with the environment
/// switch applied. Read per session, so a settings change takes effect on the
/// next conversation — the same behavior as the rest of `AppSettings`.
pub fn settings_from_app() -> TokenEfficiencySettings {
    crate::settings::load().token_efficiency.resolved()
}

/// Build the layer for a new conversation, or `None` when every feature is off.
///
/// `None` rather than an inert instance, so the hooks are never installed: a
/// layer that does nothing should not sit on the tool path taking a lock per
/// call.
pub fn layer_for_session() -> Option<Arc<TokenEfficiency>> {
    let settings = settings_from_app();
    if settings.is_inert() {
        return None;
    }
    Some(Arc::new(TokenEfficiency::new(settings)))
}

/// The process-wide metrics sink. One per process because the JSONL file is
/// append-only and shared; rows are keyed by conversation in their `turn`
/// sequence rather than by a session id, which is enough to eyeball a run and
/// keeps the layer free of session plumbing.
static METRICS: std::sync::Mutex<Option<MetricsSink>> = std::sync::Mutex::new(None);

/// Record one turn. Cheap and failure-tolerant: a metrics problem must never
/// disturb the conversation it is measuring.
pub fn record_turn(row: TurnRow) {
    let Ok(mut slot) = METRICS.lock() else {
        return;
    };
    if slot.is_none() {
        *slot = Some(metrics_sink());
    }
    if let Some(sink) = slot.as_mut() {
        sink.record(row);
    }
}

fn metrics_sink() -> MetricsSink {
    // 64 rows is enough for a UI status line and bounds memory on a long run.
    MetricsSink::with_file(MetricsSink::default_path(&crate::paths::data_dir()), 64)
}

/// A snapshot of the current session's metrics, for the UI.
pub fn metrics_summary() -> Option<Summary> {
    METRICS
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|sink| sink.summary().clone()))
}

/// The selector to use for cheap tasks (conversation titles), given the
/// conversation's own selector. Falls back to that selector when no small model
/// is configured, which reproduces the pre-routing behavior exactly.
pub fn route_title_model(primary: &str) -> String {
    let settings = settings_from_app();
    let router = Router::from_settings(Some(primary), &settings);
    router
        .route(Role::Smol)
        .map(|selector| selector.0.clone())
        .unwrap_or_else(|| primary.to_string())
}

/// Concatenate a result's text blocks. Images are ignored: the seam only
/// replaces results whose savings are textual, and never drops an image block.
pub fn join_text(content: &[TextContentOrImage]) -> String {
    let mut out = String::new();
    for block in content {
        if let TextContentOrImage::Text(text) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_efficiency::fsprobe::testing::FakeFs;

    fn layer() -> (Arc<TokenEfficiency>, Arc<FakeFs>) {
        let fs = Arc::new(FakeFs::new());
        let layer = Arc::new(TokenEfficiency::with_probe(
            TokenEfficiencySettings::default(),
            fs.clone(),
        ));
        (layer, fs)
    }

    fn text_block(text: &str) -> TextContentOrImage {
        TextContentOrImage::text(text)
    }

    #[test]
    fn join_text_combines_blocks() {
        assert_eq!(join_text(&[text_block("a"), text_block("b")]), "a\nb");
        assert_eq!(join_text(&[text_block("only")]), "only");
        assert_eq!(join_text(&[]), "");
    }

    #[test]
    fn the_after_hook_replaces_a_repeated_read() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "fn main() {}\n");
        let args = serde_json::json!({ "path": "/a.rs" });

        assert_eq!(
            hooks::observe_result(&layer, "read", &args, "fn main() {}\n", false),
            After::Continue,
            "the first read returns the body"
        );
        assert!(matches!(
            hooks::observe_result(&layer, "read", &args, "fn main() {}\n", false),
            After::Replace(stub) if stub.contains("unchanged")
        ));
    }

    #[test]
    fn the_layer_is_shared_across_hooks() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "body\n");
        let args = serde_json::json!({ "path": "/a.rs" });
        hooks::observe_result(&layer, "read", &args, "body\n", false);

        let clone = Arc::clone(&layer);
        assert!(matches!(
            hooks::observe_result(&clone, "read", &args, "body\n", false),
            After::Replace(_)
        ));
        assert_eq!(layer.counters().read_hits, 1);
    }

    #[test]
    fn turn_advancement_is_measured_in_turns() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "body\n");
        let args = serde_json::json!({ "path": "/a.rs" });

        layer.begin_turn(1);
        hooks::observe_result(&layer, "read", &args, "body\n", false);
        layer.begin_turn(2);
        assert!(matches!(
            hooks::observe_result(&layer, "read", &args, "body\n", false),
            After::Replace(stub) if stub.contains("turn 1")
        ));
    }

    #[test]
    fn routing_a_title_falls_back_to_the_primary_selector() {
        // With no `smol_model` configured, routing is a no-op. (The configured
        // case is covered in `routing`.)
        let settings = TokenEfficiencySettings::default();
        let router = Router::from_settings(Some("anthropic/claude-opus-4-7"), &settings);
        assert_eq!(
            router.route(Role::Smol).map(|selector| selector.0.clone()),
            Some("anthropic/claude-opus-4-7".to_string())
        );
    }
}
