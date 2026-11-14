//! `before_agent_start` system-prompt transform — a plugin's one chance to
//! change what the model reads for a run.
//!
//! ## Why this exists (and why it is not an event)
//!
//! pi fires `before_agent_start` and lets a handler **return** `systemPrompt`,
//! which becomes the run's `forceSystemPrompt` (`agent-session.ts:1436`). rpi
//! already broadcasts [`EventTag::BeforeAgentStart`] to observers, but the event
//! handler signature is fire-and-forget (`i32` only), so an rpi plugin could
//! watch the turn start and not influence it. This module carries the return
//! channel the broadcast cannot: an out-param handler, the same shape
//! `resources_discover` uses and for the same reason.
//!
//! ## Semantics (mirrors pi)
//!
//! - **Replacement, not append.** The returned text becomes the prompt. A plugin
//!   that wants to append reads the `"systemPrompt"` field of its input envelope
//!   and returns the concatenation — exactly the burden pi puts on extensions.
//! - **Chained in registration order.** Each handler receives the prompt the
//!   previous one returned, so a later handler can append to (or override) an
//!   earlier one's result. pi does this via a
//!   `get systemPrompt()` getter over the mutable options object
//!   (`runner.ts:1333-1347`).
//! - **A failing handler is skipped, not fatal.** Nonzero `rc` or a panic leaves
//!   the prompt as the previous handler left it and the fan-out continues, which
//!   is what pi does with a throwing handler.
//! - **For this run only.** The caller re-derives the prompt from session state
//!   each run; nothing here is persisted, matching pi's `without recording it`
//!   (`agent-session.ts:1424`).
//!
//! [`EventTag::BeforeAgentStart`]: rpi_plugin_sdk::EventTag::BeforeAgentStart

use std::panic::{catch_unwind, AssertUnwindSafe};

use rpi_plugin_sdk::{StbString, StbStringRef};

use crate::registry::{assert_active, BeforeAgentStartHandler, RegistrySnapshot};

/// Fan the `before_agent_start` prompt transform out to every registered
/// handler and return the prompt the request should carry.
///
/// `current_prompt` is the composed prompt the run would otherwise use (the
/// value a plugin sees as `systemPrompt` in its envelope). `event_json` is the
/// same envelope the [`EventTag::BeforeAgentStart`] broadcast carries, so a
/// handler sees one consistent description of the turn regardless of which
/// channel it reads.
///
/// Returns `None` when no handler changed anything — the common case, and the
/// signal to leave the prompt untouched. A stale registry (swapped-out session)
/// also returns `None`.
///
/// [`EventTag::BeforeAgentStart`]: rpi_plugin_sdk::EventTag::BeforeAgentStart
pub fn emit_before_agent_start(
    current_prompt: &str,
    event_json: &str,
    snapshot: &RegistrySnapshot,
) -> Option<String> {
    if !assert_active(snapshot.active_flag()) {
        return None;
    }
    let handlers = snapshot.before_agent_start();
    if handlers.is_empty() {
        return None;
    }

    // The envelope the handler reads. `systemPrompt` is refreshed for each
    // handler so a later one observes what an earlier one returned — the chain
    // contract. Rebuilt per iteration rather than patched in place so the
    // caller's `event_json` (which also feeds the observer event) is never
    // mutated.
    let mut prompt = current_prompt.to_string();
    let mut changed = false;

    for h in handlers {
        let envelope = with_system_prompt(event_json, &prompt);
        let envelope_ref = StbStringRef::from_str(&envelope);

        let outcome = catch_unwind(AssertUnwindSafe(|| call_one_handler(*h, envelope_ref)));
        match outcome {
            Ok(Ok(Some(next))) => {
                prompt = next;
                changed = true;
            }
            Ok(Ok(None)) => {}
            Ok(Err(rc)) => {
                tracing::warn!(
                    rc,
                    "before_agent_start handler returned nonzero — prompt left as-is \
                     (fan-out continues)"
                );
            }
            Err(_) => {
                tracing::error!(
                    "before_agent_start handler panicked — prompt left as-is \
                     (fan-out continues)"
                );
            }
        }
    }

    changed.then_some(prompt)
}

/// Replace (or add) the `systemPrompt` field of an event envelope.
///
/// Falls back to a minimal envelope when `event_json` is not a JSON object, so a
/// malformed base can never leave a handler without the field it needs to append
/// to.
fn with_system_prompt(event_json: &str, prompt: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(event_json) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.insert(
                "systemPrompt".to_string(),
                serde_json::Value::String(prompt.to_string()),
            );
            serde_json::Value::Object(map).to_string()
        }
        _ => serde_json::json!({ "systemPrompt": prompt }).to_string(),
    }
}

/// Call one handler and extract its replacement prompt.
///
/// `Ok(None)` means "no change" (an empty `out`, an empty object, or a missing
/// `systemPrompt` key) — the same as pi reading
/// `result?.systemPrompt !== undefined`. `Err(rc)` is a handled plugin error.
fn call_one_handler(
    h: BeforeAgentStartHandler,
    event_ref: StbStringRef,
) -> Result<Option<String>, i32> {
    // The uninitialized `out` slot the handler writes into on success. Contract
    // matches `resources_discover`: `rc==0` ⇒ `out` is a plugin-owned JSON string
    // the host frees; `rc!=0` ⇒ the handler wrote nothing. Zeroed so a `rc==0`
    // handler that forgets to write yields an empty parse rather than UB.
    let mut out = StbString::empty();
    let rc = (h.handler)(event_ref, &mut out, h.user_data);
    // SAFETY: the callback transfers its live output to its registered producer.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(h.plugin_free_string))
    };
    if rc != 0 {
        // The owner also reclaims output a handler wrote before returning error.
        return Err(rc);
    }

    // Copy the plugin-owned bytes into a safe String, THEN free the plugin
    // allocation — we must not keep a reference past the free.
    let json = out.to_string_lossy();

    Ok(parse_prompt_payload(&json))
}

/// Read `{"systemPrompt": "..."}` out of a handler's output.
///
/// Anything else — empty, malformed, a non-object, a missing or non-string
/// `systemPrompt` — means "no change", so a handler that only wants to observe
/// can return an empty string and be correct. An explicit empty *string* is
/// honored as a change (a plugin can legitimately clear the prompt), which is
/// why the presence of the key is checked rather than its emptiness.
fn parse_prompt_payload(json: &str) -> Option<String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    value.get("systemPrompt")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_the_string_and_ignores_everything_else() {
        assert_eq!(
            parse_prompt_payload(r#"{"systemPrompt":"hi"}"#),
            Some("hi".to_string())
        );
        // An explicitly empty prompt IS a change (a plugin may clear it).
        assert_eq!(
            parse_prompt_payload(r#"{"systemPrompt":""}"#),
            Some(String::new())
        );
        // Absent / wrong type / malformed / empty ⇒ no change.
        assert_eq!(parse_prompt_payload("{}"), None);
        assert_eq!(parse_prompt_payload(r#"{"systemPrompt":42}"#), None);
        assert_eq!(parse_prompt_payload("not json"), None);
        assert_eq!(parse_prompt_payload(""), None);
        assert_eq!(parse_prompt_payload("   "), None);
    }

    #[test]
    fn envelope_carries_the_prompt_and_survives_a_bad_base() {
        let base = serde_json::json!({"prompt": "do a thing", "imageCount": 0}).to_string();
        let out = with_system_prompt(&base, "you are terse");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // The original fields survive — a handler that appends still sees them.
        assert_eq!(v["prompt"], "do a thing");
        assert_eq!(v["systemPrompt"], "you are terse");

        // A non-object base must still yield a usable envelope.
        let out = with_system_prompt("[]", "p");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["systemPrompt"], "p");

        // Each chained call refreshes the field, so handler N sees N-1's result.
        let second = with_system_prompt(&out, "updated");
        let v2: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(v2["systemPrompt"], "updated");
    }
}
