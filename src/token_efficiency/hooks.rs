//! The layer's two integration surfaces, written against plain data.
//!
//! # Why this module has no `agent::` types in it
//!
//! The hooks the harness calls (`BeforeToolCall`, `AfterToolCall`) take
//! `&AgentContext` and `&AgentToolResult`, which are rpi types. Those belong in
//! the wiring seam (`mod.rs`), not in the logic that decides whether a read is
//! unchanged. Keeping the decision here as pure functions over
//! `(tool name, arguments, text)` means the cache, the anchor expander and the
//! minimizer can be tested without building a harness, and the seam stays small
//! enough to audit at a glance.
//!
//! # The three decisions
//!
//! - [`intercept`] runs *before* a tool: it rewrites arguments (anchor edits →
//!   `edits[]`) or answers the call from the search cache.
//! - [`observe_result`] runs *after* a tool: it may replace a read body with an
//!   "unchanged since turn N" stub, minimize noisy output, and record the stamps
//!   for the next turn.
//! - [`note_write`] invalidates after `write`/`edit`.
//!
//! Each returns a value the seam maps onto `BeforeToolCallResult` /
//! `AfterToolCallResult` field by field. No decision here mutates the cache
//! except through the `ToolCache` methods that own the invalidation rules.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::anchors::{self, AnchorPatch};
use super::cache::{CacheCounters, ContentStamp, Lookup, ReadKey, ReadRange, SearchKey, ToolCache};
use super::config::TokenEfficiencySettings;
use super::fsprobe::{FileProbe, OsFileProbe};
use super::minimize;

/// Shared state for one conversation.
pub struct TokenEfficiency {
    cache: Mutex<ToolCache>,
    probe: Arc<dyn FileProbe>,
    settings: TokenEfficiencySettings,
    /// Set by [`observe_result`] for the next `after_tool_call` accounting pass.
    turn_bytes_saved: Mutex<u64>,
}

impl TokenEfficiency {
    pub fn new(settings: TokenEfficiencySettings) -> Self {
        Self::with_probe(settings, Arc::new(OsFileProbe))
    }

    pub fn with_probe(settings: TokenEfficiencySettings, probe: Arc<dyn FileProbe>) -> Self {
        let settings = settings.resolved();
        Self {
            cache: Mutex::new(ToolCache::new(settings.clone())),
            probe,
            settings,
            turn_bytes_saved: Mutex::new(0),
        }
    }

    pub fn settings(&self) -> &TokenEfficiencySettings {
        &self.settings
    }

    pub fn counters(&self) -> CacheCounters {
        self.lock().counters()
    }

    /// Advance the conversation clock. Called from the seam on each provider
    /// turn so the dedupe window is measured in turns, not wall time.
    pub fn begin_turn(&self, turn: u64) {
        self.lock().begin_turn(turn);
    }

    /// Bytes kept out of the prompt since the last [`Self::take_bytes_saved`].
    pub fn take_bytes_saved(&self) -> u64 {
        let mut saved = self
            .turn_bytes_saved
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *saved)
    }

    /// Compaction, session resume, or a deliberate context rewrite: nothing the
    /// cache recorded is still visible to the model.
    pub fn invalidate_all(&self) {
        self.lock().invalidate_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ToolCache> {
        self.cache.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn add_saved(&self, bytes: usize) {
        let mut saved = self
            .turn_bytes_saved
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *saved += bytes as u64;
    }
}

/// What the seam should do with a tool call, before it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Before {
    /// Run as-is.
    Continue,
    /// Run with these arguments instead.
    Rewrite(Value),
    /// Do not run; this text is the result.
    ShortCircuit(String),
}

/// One post-execution outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum After {
    /// Leave the result alone.
    Continue,
    /// Replace the result's text content.
    Replace(String),
}

// ---- before -------------------------------------------------------------

/// Decide what to do with `(tool, args)` before execution.
pub fn intercept(layer: &TokenEfficiency, tool: &str, args: &Value) -> Before {
    if !layer.settings.enabled {
        return Before::Continue;
    }

    if layer.settings.anchor_edits && tool == "edit" {
        match try_anchor_edit(layer, args) {
            Some(before) => return before,
            None => return Before::Continue,
        }
    }

    if layer.settings.search_cache
        && matches!(tool, "grep" | "find" | "ls")
        && let Some(hit) = try_search_hit(layer, tool, args)
    {
        return Before::ShortCircuit(hit);
    }

    Before::Continue
}

/// Translate an anchor-form `edit` into the `edits[]` form the tool already
/// understands. Returns `None` for ordinary (non-anchor) edits, so the existing
/// path is untouched.
fn try_anchor_edit(layer: &TokenEfficiency, args: &Value) -> Option<Before> {
    let path = args.get("path")?.as_str()?;
    let anchor = args.get("anchor").and_then(Value::as_str)?;
    let ops = match args.get("ops") {
        Some(Value::String(body)) => match anchors::parse(body) {
            Ok(patch) => patch.ops,
            Err(error) => return Some(Before::ShortCircuit(error.to_string())),
        },
        Some(Value::Array(_)) => {
            // An explicit op array is not part of the documented shape; refuse
            // rather than guess, because a wrong edit is worse than a retry.
            return Some(Before::ShortCircuit(
                "`ops` must be a string patch body (PUT/CUT headers), not an array".into(),
            ));
        }
        _ => {
            return Some(Before::ShortCircuit(
                "anchor edits need `ops`: a patch body using PUT/CUT headers".into(),
            ));
        }
    };

    let raw = PathBuf::from(path);
    let canonical = layer.probe.canonicalize(&raw);
    let snapshot = match layer.probe.snapshot(&canonical) {
        Some(snapshot) => snapshot,
        None => {
            return Some(Before::ShortCircuit(format!(
                "cannot read {path}; anchor edits need the file on disk"
            )));
        }
    };
    let current = ContentStamp::of_bytes(&snapshot.bytes);
    let content = String::from_utf8_lossy(&snapshot.bytes).into_owned();

    let patch = AnchorPatch {
        tag: anchor.to_string(),
        ops: ops.clone(),
    };
    let expansion = {
        let mut cache = layer.lock();
        let ledger = cache.anchor(&canonical);
        // Verify inside one lock acquisition so a concurrent delivery cannot
        // change the ledger between the check and the counter it updates.
        let verified = anchors::verify(path, &patch, ledger, current);
        match verified {
            Ok(()) => {
                cache.record_anchor_expansion();
                anchors::expand(&content, &ops)
            }
            Err(error) => {
                if matches!(
                    error,
                    anchors::AnchorError::Stale { .. } | anchors::AnchorError::Moved { .. }
                ) {
                    cache.record_anchor_stale();
                }
                return Some(Before::ShortCircuit(error.to_string()));
            }
        }
    };

    let edits = match expansion {
        Ok(edits) => edits,
        Err(error) => return Some(Before::ShortCircuit(error.to_string())),
    };

    // The savings this buys: the model sent `PUT n.==m:` plus new lines instead
    // of the pre-image. Count what it did not have to send.
    let pre_image: usize = edits.iter().map(|edit| edit.old_text.len()).sum();
    let op_body: usize = anchor.len() + ops.len();
    layer.add_saved(pre_image.saturating_sub(op_body));

    let edits: Vec<Value> = edits
        .into_iter()
        .map(|edit| {
            json!({
                "oldText": edit.old_text,
                "newText": edit.new_text,
            })
        })
        .collect();

    // Preserve any other keys the model sent (none are documented, but dropping
    // an unknown field silently would be a worse failure than passing it on).
    let mut rewritten = args.clone();
    if let Some(object) = rewritten.as_object_mut() {
        object.remove("anchor");
        object.remove("ops");
        object.insert("edits".into(), Value::Array(edits));
    }
    Some(Before::Rewrite(rewritten))
}

fn try_search_hit(layer: &TokenEfficiency, tool: &str, args: &Value) -> Option<String> {
    let key = search_key(tool, args)?;
    let cached = layer.lock().observe_search(layer.probe.as_ref(), &key)?;
    layer.add_saved(cached.result_bytes);
    Some(format!(
        "[search cache hit: identical request, and no file it scanned changed since turn {}. \
         {} match{} (same result as before — nothing new to find). \
         Re-run with {{\"fresh\": true}} to force a new scan.]",
        cached.turn,
        cached.match_count,
        if cached.match_count == 1 { "" } else { "es" },
    ))
}

/// Build a search key from tool arguments, or `None` when the call is not
/// cacheable (an explicit `fresh`, or a shape this module does not model).
pub fn search_key(tool: &str, args: &Value) -> Option<SearchKey> {
    if args.get("fresh").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .or_else(|| args.get("glob").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let root = args
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    Some(SearchKey {
        tool: tool.to_string(),
        pattern,
        root,
        glob: args.get("glob").and_then(Value::as_str).map(str::to_string),
        ignore_case: args
            .get("ignoreCase")
            .or_else(|| args.get("ignore_case"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        literal: args
            .get("literal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit: args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|limit| limit as u32),
    })
}

// ---- after --------------------------------------------------------------

/// Decide what to do with a completed tool result.
///
/// `text` is the result's concatenated text content; images are the caller's
/// problem (this layer never rewrites or drops an image).
pub fn observe_result(
    layer: &TokenEfficiency,
    tool: &str,
    args: &Value,
    text: &str,
    is_error: bool,
) -> After {
    if !layer.settings.enabled || is_error {
        // A failed call is a different answer than the successful one it is
        // being compared against; never claim "unchanged" for an error.
        return After::Continue;
    }

    if tool == "read" && layer.settings.read_dedupe {
        return observe_read(layer, args, text);
    }

    // Minimize first, then record: the search entry must describe the answer the
    // model actually received, not the pre-compression buffer.
    if layer.settings.minimize_output && minimize::is_minimizable_tool(tool) {
        match maybe_minimize(layer, text) {
            After::Replace(minimized) => {
                if layer.settings.search_cache && matches!(tool, "grep" | "find" | "ls") {
                    record_search(layer, tool, args, &minimized);
                }
                return After::Replace(minimized);
            }
            After::Continue => {}
        }
    }

    if layer.settings.search_cache && matches!(tool, "grep" | "find" | "ls") {
        record_search(layer, tool, args, text);
    }

    After::Continue
}

fn observe_read(layer: &TokenEfficiency, args: &Value, text: &str) -> After {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return After::Continue;
    };
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .map(|offset| offset as usize)
        .unwrap_or(1);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|l| l as usize);
    let fresh = args.get("fresh").and_then(Value::as_bool).unwrap_or(false);

    let range = ReadRange {
        start: offset.saturating_sub(1),
        end: limit.map(|limit| offset.saturating_sub(1).saturating_add(limit)),
    };
    let key = ReadKey::new(path, range, fresh);

    let (lookup, previous) =
        layer
            .lock()
            .observe_read(layer.probe.as_ref(), Path::new(path), key, text.len());

    match lookup {
        Lookup::Hit => {
            let delivered = previous.expect("a hit implies a previous delivery");
            layer.add_saved(delivered.delivered_bytes);
            After::Replace(stub_text(path, delivered))
        }
        _ => After::Continue,
    }
}

/// The stub that replaces a repeated body. It names the turn and the size so the
/// model can judge whether it needs the content again, and it says how to get it.
fn stub_text(path: &str, delivered: super::cache::DeliveredRead) -> String {
    format!(
        "[unchanged since turn {}: {path} — {} lines, {}, sha {}. \
         Content is identical to that read; re-issue read with {{\"fresh\": true}} to force a resend.]",
        delivered.turn,
        delivered.stamp.lines,
        human_bytes(delivered.stamp.len as usize),
        delivered.stamp.digest.tag(8),
    )
}

fn record_search(layer: &TokenEfficiency, tool: &str, args: &Value, text: &str) {
    let Some(key) = search_key(tool, args) else {
        return;
    };
    // A result with an elision marker is not the complete answer, and caching it
    // would let a later identical call miss new matches hidden past the cap.
    let capped = text.contains("lines elided") || text.contains("Limit reached");
    if capped {
        return;
    }
    // Record every file the result mentions as "scanned" — the search's own
    // output is the authoritative list of files it read, and it is free.
    let mut scanned: Vec<(PathBuf, i64)> = Vec::new();
    for line in text.lines() {
        let candidate = line.split(':').next().unwrap_or("").trim();
        if candidate.is_empty() || !candidate.contains('/') && !candidate.contains('.') {
            continue;
        }
        let path = PathBuf::from(candidate);
        let canonical = layer.probe.canonicalize(&path);
        if let Some(meta) = layer.probe.metadata(&canonical) {
            scanned.push((canonical, meta.mtime_ms));
        }
    }
    scanned.sort();
    scanned.dedup();
    if scanned.is_empty() {
        return;
    }
    let matches = text.lines().filter(|line| !line.trim().is_empty()).count();
    layer
        .lock()
        .record_search(key, scanned, text.as_bytes(), matches);
}

fn maybe_minimize(layer: &TokenEfficiency, text: &str) -> After {
    if text.len() <= layer.settings.minimize_over_bytes {
        return After::Continue;
    }
    let (out, stats) = minimize::minimize(text, layer.settings.minimize_max_bytes);
    if stats.saved() == 0 {
        return After::Continue;
    }
    layer.add_saved(stats.saved());
    After::Replace(out)
}

// ---- write invalidation -------------------------------------------------

/// A mutating tool changed `args.path`; drop everything that described the old
/// bytes.
pub fn note_write(layer: &TokenEfficiency, tool: &str, args: &Value) {
    if !matches!(tool, "write" | "edit") {
        return;
    }
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return;
    };
    let canonical = layer.probe.canonicalize(Path::new(path));
    layer.lock().invalidate_path(&canonical);
}

fn human_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    if bytes < KIB {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / KIB as f64;
    if kib < KIB as f64 {
        return format!("{kib:.1} KiB");
    }
    format!("{:.1} MiB", kib / KIB as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_efficiency::fsprobe::testing::FakeFs;

    fn layer_with(settings: TokenEfficiencySettings) -> (TokenEfficiency, Arc<FakeFs>) {
        let fs = Arc::new(FakeFs::new());
        let layer = TokenEfficiency::with_probe(settings, fs.clone());
        (layer, fs)
    }

    fn layer() -> (TokenEfficiency, Arc<FakeFs>) {
        layer_with(TokenEfficiencySettings::default())
    }

    fn read_args(path: &str) -> Value {
        json!({ "path": path })
    }

    // ---- read dedupe -----------------------------------------------------

    #[test]
    fn a_repeated_read_is_replaced_by_a_stub() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "fn main() {}\n");
        layer.begin_turn(3);

        let first = observe_result(&layer, "read", &read_args("/a.rs"), "fn main() {}\n", false);
        assert_eq!(
            first,
            After::Continue,
            "the first read must return the body"
        );

        let second = observe_result(&layer, "read", &read_args("/a.rs"), "fn main() {}\n", false);
        match second {
            After::Replace(stub) => {
                assert!(
                    stub.starts_with("[unchanged since turn 3: /a.rs"),
                    "got {stub:?}"
                );
                assert!(stub.contains("sha "));
                assert!(stub.contains("fresh"));
                assert!(stub.len() < 40 + "/a.rs".len() + 120);
            }
            other => panic!("expected a stub, got {other:?}"),
        }
    }

    #[test]
    fn an_edited_file_is_returned_in_full_again() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "one\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "one\n", false);

        fs.write("/a.rs", "two\n");
        let after_edit = observe_result(&layer, "read", &read_args("/a.rs"), "two\n", false);
        assert_eq!(
            after_edit,
            After::Continue,
            "changed content is never stubbed"
        );
    }

    #[test]
    fn a_write_tool_invalidates_before_the_next_read() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "one\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "one\n", false);

        note_write(&layer, "edit", &json!({ "path": "/a.rs" }));
        fs.write("/a.rs", "one\n");
        let next = observe_result(&layer, "read", &read_args("/a.rs"), "one\n", false);
        assert_eq!(next, After::Continue, "an edit must force a real re-read");
    }

    #[test]
    fn an_error_result_is_never_stubbed() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "body\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        let failed = observe_result(&layer, "read", &read_args("/a.rs"), "body\n", true);
        assert_eq!(failed, After::Continue);
    }

    #[test]
    fn a_fresh_read_escapes_the_stub() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "body\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        let forced = observe_result(
            &layer,
            "read",
            &json!({ "path": "/a.rs", "fresh": true }),
            "body\n",
            false,
        );
        assert_eq!(forced, After::Continue);
    }

    #[test]
    fn a_ranged_read_does_not_collide_with_a_full_read() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "l1\nl2\nl3\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "l1\nl2\nl3\n", false);
        let ranged = observe_result(
            &layer,
            "read",
            &json!({ "path": "/a.rs", "offset": 2, "limit": 1 }),
            "l2\n",
            false,
        );
        assert_eq!(
            ranged,
            After::Continue,
            "a different range is a different answer"
        );
    }

    #[test]
    fn compaction_invalidates_every_stub() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "body\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        layer.invalidate_all();
        let next = observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        assert_eq!(
            next,
            After::Continue,
            "after compaction the body is gone from context"
        );
    }

    #[test]
    fn savings_are_reported_per_turn() {
        let (layer, fs) = layer();
        fs.write("/a.rs", &"x".repeat(2000));
        let body = "x".repeat(2000);
        observe_result(&layer, "read", &read_args("/a.rs"), &body, false);
        assert_eq!(layer.take_bytes_saved(), 0);

        observe_result(&layer, "read", &read_args("/a.rs"), &body, false);
        assert_eq!(layer.take_bytes_saved(), 2000);
        assert_eq!(layer.take_bytes_saved(), 0, "taking resets the counter");
    }

    #[test]
    fn read_dedupe_off_does_nothing() {
        let (layer, fs) = layer_with(TokenEfficiencySettings {
            read_dedupe: false,
            ..TokenEfficiencySettings::default()
        });
        fs.write("/a.rs", "body\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        let again = observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        assert_eq!(again, After::Continue);
    }

    #[test]
    fn the_whole_layer_can_be_switched_off() {
        let (layer, fs) = layer_with(TokenEfficiencySettings {
            enabled: false,
            ..TokenEfficiencySettings::default()
        });
        fs.write("/a.rs", "body\n");
        observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false);
        assert_eq!(
            observe_result(&layer, "read", &read_args("/a.rs"), "body\n", false),
            After::Continue
        );
        assert_eq!(
            intercept(&layer, "grep", &json!({"pattern": "x"})),
            Before::Continue
        );
    }

    // ---- anchors ---------------------------------------------------------

    #[test]
    fn an_anchor_edit_becomes_an_edits_array() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "one\ntwo\nthree");
        let body = "one\ntwo\nthree";
        observe_result(&layer, "read", &read_args("/a.rs"), body, false);
        let tag = layer
            .lock()
            .anchor(Path::new("/a.rs"))
            .expect("ledger")
            .stamp
            .digest
            .tag(4);

        let before = intercept(
            &layer,
            "edit",
            &json!({
                "path": "/a.rs",
                "anchor": tag,
                "ops": "PUT 2.==2:\nTWO"
            }),
        );
        match before {
            Before::Rewrite(args) => {
                assert!(args.get("anchor").is_none(), "the anchor must not survive");
                assert!(args.get("ops").is_none());
                let edits = args["edits"].as_array().expect("edits array");
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0]["oldText"], json!("two"));
                assert_eq!(edits[0]["newText"], json!("TWO"));
                assert_eq!(args["path"], json!("/a.rs"));
            }
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_anchor_is_refused_with_the_current_tag() {
        let (layer, fs) = layer();
        fs.write("/a.rs", "one\ntwo");
        observe_result(&layer, "read", &read_args("/a.rs"), "one\ntwo", false);
        // The file changes after the read.
        fs.write("/a.rs", "one\ntwo\nthree");

        let before = intercept(
            &layer,
            "edit",
            &json!({ "path": "/a.rs", "anchor": "0000", "ops": "PUT 1:\nONE" }),
        );
        match before {
            Before::ShortCircuit(message) => assert!(message.contains("stale"), "got {message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(layer.counters().anchor_stale, 1);
    }

    #[test]
    fn an_anchor_for_an_unread_file_is_refused() {
        let (layer, fs) = layer();
        fs.write("/never-read.rs", "x");
        let before = intercept(
            &layer,
            "edit",
            &json!({ "path": "/never-read.rs", "anchor": "abcd", "ops": "PUT 1:\ny" }),
        );
        match before {
            Before::ShortCircuit(message) => assert!(message.contains("has not been read")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_anchor_edit_to_a_missing_file_is_refused() {
        let (layer, _fs) = layer();
        let before = intercept(
            &layer,
            "edit",
            &json!({ "path": "/gone.rs", "anchor": "abcd", "ops": "PUT 1:\ny" }),
        );
        assert!(matches!(before, Before::ShortCircuit(_)));
    }

    #[test]
    fn a_plain_edit_is_left_alone() {
        let (layer, _fs) = layer();
        let args = json!({ "path": "/a.rs", "edits": [{ "oldText": "a", "newText": "b" }] });
        assert_eq!(intercept(&layer, "edit", &args), Before::Continue);
    }

    #[test]
    fn an_anchor_edit_saves_the_pre_image_bytes() {
        let (layer, fs) = layer();
        let body = "one\ntwo\nthree\nfour";
        fs.write("/a.rs", body);
        observe_result(&layer, "read", &read_args("/a.rs"), body, false);
        let tag = layer
            .lock()
            .anchor(Path::new("/a.rs"))
            .expect("ledger")
            .stamp
            .digest
            .tag(4);
        layer.take_bytes_saved();

        let ops = "PUT 2.==3:\nTWO\nTHREE";
        intercept(
            &layer,
            "edit",
            &json!({ "path": "/a.rs", "anchor": tag, "ops": ops }),
        );
        // The model sent the header plus the new text; it did not send
        // "two\nthree". That difference is the saving.
        let saved = layer.take_bytes_saved();
        assert!(saved > 0, "the pre-image it did not send must be counted");
    }

    #[test]
    fn anchor_edits_off_leaves_the_call_alone() {
        let (layer, fs) = layer_with(TokenEfficiencySettings {
            anchor_edits: false,
            ..TokenEfficiencySettings::default()
        });
        fs.write("/a.rs", "one");
        observe_result(&layer, "read", &read_args("/a.rs"), "one", false);
        let before = intercept(
            &layer,
            "edit",
            &json!({ "path": "/a.rs", "anchor": "abcd", "ops": "PUT 1:\nx" }),
        );
        assert_eq!(before, Before::Continue);
    }

    // ---- search ----------------------------------------------------------

    #[test]
    fn an_identical_search_is_short_circuited() {
        let (layer, fs) = layer();
        fs.write("/work/a.rs", "fn one() {}\n");
        let args = json!({ "pattern": "fn ", "path": "/work/a.rs" });
        let result = "/work/a.rs:1:fn one() {}";
        assert_eq!(
            observe_result(&layer, "grep", &args, result, false),
            After::Continue
        );

        match intercept(&layer, "grep", &args) {
            Before::ShortCircuit(message) => {
                assert!(message.contains("search cache hit"), "got {message}");
                assert!(message.contains("turn 0"));
                assert!(message.contains("fresh"));
            }
            other => panic!("expected a short-circuit, got {other:?}"),
        }
    }

    #[test]
    fn a_search_is_short_circuited_only_while_its_files_are_unchanged() {
        let (layer, fs) = layer();
        fs.write("/work/a.rs", "fn one() {}\n");
        let args = json!({ "pattern": "fn ", "path": "/work/a.rs" });
        observe_result(&layer, "grep", &args, "/work/a.rs:1:fn one() {}", false);
        assert!(matches!(
            intercept(&layer, "grep", &args),
            Before::ShortCircuit(_)
        ));

        fs.write("/work/a.rs", "fn one() {}\nfn two() {}\n");
        assert_eq!(intercept(&layer, "grep", &args), Before::Continue);
    }

    #[test]
    fn a_capped_search_result_is_not_cached() {
        let (layer, fs) = layer();
        fs.write("/work/a.rs", "x\n");
        let args = json!({ "pattern": "x", "path": "/work/a.rs" });
        observe_result(
            &layer,
            "grep",
            &args,
            "/work/a.rs:1:x\n[…9 lines elided to keep this result under 24576 bytes]",
            false,
        );
        assert_eq!(intercept(&layer, "grep", &args), Before::Continue);
    }

    #[test]
    fn a_fresh_search_bypasses_the_cache_both_ways() {
        let (layer, fs) = layer();
        fs.write("/work/a.rs", "fn one() {}\n");
        let args = json!({ "pattern": "fn ", "path": "/work/a.rs" });
        observe_result(&layer, "grep", &args, "/work/a.rs:1:fn one() {}", false);

        let fresh = json!({ "pattern": "fn ", "path": "/work/a.rs", "fresh": true });
        assert_eq!(intercept(&layer, "grep", &fresh), Before::Continue);
        // And a fresh run does not overwrite the cached entry with a duplicate.
        observe_result(&layer, "grep", &fresh, "/work/a.rs:1:fn one() {}", false);
        assert!(matches!(
            intercept(&layer, "grep", &args),
            Before::ShortCircuit(_)
        ));
    }

    #[test]
    fn search_args_that_change_the_answer_do_not_hit() {
        let (layer, fs) = layer();
        fs.write("/work/a.rs", "fn one() {}\n");
        observe_result(
            &layer,
            "grep",
            &json!({ "pattern": "fn ", "path": "/work/a.rs" }),
            "/work/a.rs:1:fn one() {}",
            false,
        );
        assert_eq!(
            intercept(
                &layer,
                "grep",
                &json!({ "pattern": "other", "path": "/work/a.rs" })
            ),
            Before::Continue
        );
        assert_eq!(
            intercept(
                &layer,
                "grep",
                &json!({ "pattern": "fn ", "path": "/work/a.rs", "ignoreCase": true })
            ),
            Before::Continue
        );
    }

    #[test]
    fn search_key_accepts_camel_and_snake_case() {
        let camel = search_key("grep", &json!({ "pattern": "a", "ignoreCase": true })).unwrap();
        let snake = search_key("grep", &json!({ "pattern": "a", "ignore_case": true })).unwrap();
        assert!(camel.ignore_case);
        assert!(snake.ignore_case);
    }

    // ---- minimization ----------------------------------------------------

    #[test]
    fn noisy_bash_output_is_minimized_and_measured() {
        let (layer, _fs) = layer();
        let text = format!("{}\nDone\n", "Compiling a".repeat_same(400));
        let after = observe_result(&layer, "bash", &json!({}), &text, false);
        match after {
            After::Replace(out) => {
                assert!(out.len() < text.len());
                assert!(out.contains("×400") || out.contains("×"));
            }
            other => panic!("expected minimization, got {other:?}"),
        }
        assert!(layer.take_bytes_saved() > 0);
    }

    #[test]
    fn small_output_is_untouched() {
        let (layer, _fs) = layer();
        assert_eq!(
            observe_result(&layer, "bash", &json!({}), "ok\n", false),
            After::Continue
        );
    }

    #[test]
    fn read_output_is_never_minimized() {
        let (layer, fs) = layer();
        let body = "x\n".repeat(5000);
        fs.write("/big.txt", &body);
        let after = observe_result(&layer, "read", &read_args("/big.txt"), &body, false);
        assert_eq!(
            after,
            After::Continue,
            "read's own truncation is authoritative"
        );
    }

    #[test]
    fn minimize_off_leaves_output_alone() {
        let (layer, _fs) = layer_with(TokenEfficiencySettings {
            minimize_output: false,
            ..TokenEfficiencySettings::default()
        });
        let text = "y\n".repeat(5000);
        assert_eq!(
            observe_result(&layer, "bash", &json!({}), &text, false),
            After::Continue
        );
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    /// `"s".repeat(n)` joined by newlines, without pulling in a helper crate.
    trait RepeatSame {
        fn repeat_same(self, count: usize) -> String;
    }

    impl RepeatSame for &str {
        fn repeat_same(self, count: usize) -> String {
            (0..count).map(|_| self).collect::<Vec<_>>().join("\n")
        }
    }
}
