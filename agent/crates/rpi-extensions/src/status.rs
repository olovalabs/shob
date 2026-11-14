//! Extension status line.
//!
//! A tiny host-side registry that lets an extension publish a short status
//! string (e.g. `langfuse ✓ (trace sent)`) for the TUI to render in its footer.
//! Mirrors [`UiDialogMailbox`](crate::UiDialogMailbox): the plugin writes
//! through the `SetStatus` runtime action, and the TUI — the only consumer —
//! polls [`ExtensionStatusMailbox::revision`] once per render tick and repaints
//! only when something actually changed.
//!
//! Keyed by extension name so several extensions can report at once; keys are
//! rendered in sorted order, which keeps the line stable frame to frame. An
//! empty value clears that key. Headless runs simply never read the registry —
//! writing to it is always cheap and never fails.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared, cloneable handle to the extension status registry.
///
/// Every clone refers to the same map: the host hands one clone to the action
/// bridge (plugin writes) and one to the TUI (rendering).
#[derive(Clone, Default)]
pub struct ExtensionStatusMailbox {
    entries: Arc<Mutex<BTreeMap<String, String>>>,
    components: Arc<Mutex<BTreeMap<(u64, String), (crate::ExtensionPanel, bool)>>>,
    /// Bumped on every accepted write so readers can skip unchanged frames.
    revision: Arc<AtomicU64>,
}

impl ExtensionStatusMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or, with an empty `value`, clear) one extension's status text.
    ///
    /// Returns `true` when the map actually changed — the caller can use that
    /// to avoid a redundant repaint.
    pub fn set(&self, key: &str, value: &str) -> bool {
        let key = key.trim();
        if key.is_empty() {
            return false;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let changed = if value.trim().is_empty() {
            entries.remove(key).is_some()
        } else {
            entries.insert(key.to_string(), value.to_string()) != Some(value.to_string())
        };
        if changed {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        changed
    }

    /// Handle the legacy SetStatus footer action.
    pub fn handle(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let key = args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or("set_status requires a string `key`")?;
        if args.get("panel").is_some() {
            return Err("panel was removed; use UiComponent".into());
        }
        let value = args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let changed = self.set(key, value);
        Ok(serde_json::json!({ "ok": true, "changed": changed }))
    }

    pub fn component(
        &self,
        owner: u64,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = args["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 128)
            .ok_or("invalid component id")?;
        let key = (owner, id.to_owned());
        let op = args["op"].as_str().ok_or("missing component operation")?;
        let mut entries = self
            .components
            .lock()
            .map_err(|_| "component registry unavailable")?;
        let before = entries.get(&key).cloned();
        let allowed: &[&str] = match op {
            "register" => &[
                "op",
                "id",
                "type",
                "slot",
                "order",
                "visible",
                "anchor",
                "offsetX",
                "offsetY",
                "width",
                "maxHeight",
                "minScreenWidth",
                "border",
            ],
            "configure" => &[
                "op",
                "id",
                "slot",
                "order",
                "anchor",
                "offsetX",
                "offsetY",
                "width",
                "maxHeight",
                "minScreenWidth",
                "border",
            ],
            "update" => &["op", "id", "text", "title", "highlights"],
            _ => &["op", "id"],
        };
        if args
            .as_object()
            .ok_or("component payload must be an object")?
            .keys()
            .any(|k| !allowed.contains(&k.as_str()))
        {
            return Err("unexpected component field".into());
        }
        let next = match op {
            "register" if before.is_some() => before.clone(),
            "remove" => None,
            "register" | "configure" | "update" | "show" | "hide" => {
                let (mut spec, mut visible) = if op == "register" {
                    if entries.keys().filter(|(o, _)| *o == owner).count() >= 32 {
                        return Err("too many components".into());
                    }
                    if args.get("type").and_then(|v| v.as_str()).unwrap_or("text") != "text" {
                        return Err("unsupported component type".into());
                    }
                    (
                        serde_json::json!({"version":1,"lines":[]}),
                        args["visible"].as_bool().unwrap_or(true),
                    )
                } else {
                    let (panel, visible) = before.clone().ok_or("component is not registered")?;
                    (
                        serde_json::to_value(panel).map_err(|e| e.to_string())?,
                        visible,
                    )
                };
                if op == "register" || op == "configure" {
                    if let Some(slot) = args.get("slot") {
                        let (layout, anchor) = match slot.as_str().ok_or("invalid slot")? {
                            "footer.after" => ("footer", "top-right"),
                            "sidebar.left" => ("sidebar", "top-left"),
                            "sidebar.right" => ("sidebar", "top-right"),
                            "overlay" => ("overlay", "top-right"),
                            _ => return Err("unsupported component slot".into()),
                        };
                        spec["layout"] = layout.into();
                        spec["anchor"] = anchor.into();
                    }
                    for field in [
                        "order",
                        "anchor",
                        "offsetX",
                        "offsetY",
                        "width",
                        "maxHeight",
                        "minScreenWidth",
                        "border",
                    ] {
                        if let Some(v) = args.get(field) {
                            spec[field] = v.clone();
                        }
                    }
                } else if op == "update" {
                    if let Some(v) = args.get("text") {
                        spec["lines"] = serde_json::json!(v
                            .as_str()
                            .ok_or("text must be a string")?
                            .split('\n')
                            .collect::<Vec<_>>());
                        // Plain text updates clear previous styling.
                        spec["highlights"] = serde_json::json!([]);
                    }
                    if let Some(v) = args.get("highlights") {
                        spec["highlights"] = v.clone();
                    }
                    if let Some(v) = args.get("title") {
                        spec["title"] = v.as_str().ok_or("title must be a string")?.into();
                    }
                } else {
                    visible = op == "show";
                }
                let panel: crate::ExtensionPanel =
                    serde_json::from_value(spec).map_err(|e| e.to_string())?;
                panel.validate()?;
                Some((panel, visible))
            }
            _ => return Err("unknown component operation".into()),
        };
        let changed = before != next;
        if changed {
            match next {
                Some(v) => {
                    entries.insert(key, v);
                }
                None => {
                    entries.remove(&key);
                }
            }
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(serde_json::json!({"ok":true,"changed":changed}))
    }
    pub fn remove_owner(&self, owner: u64) {
        if let Ok(mut entries) = self.components.lock() {
            let size = entries.len();
            entries.retain(|(o, _), _| *o != owner);
            if size != entries.len() {
                self.revision.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Monotonic write counter; changes whenever any entry changed.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// Read one entry without exposing the mutable registry.
    pub fn get(&self, key: &str) -> Option<String> {
        self.entries.lock().ok()?.get(key).cloned()
    }

    /// Snapshot declarative panels separately from the legacy footer registry.
    pub fn panels(&self) -> BTreeMap<String, crate::ExtensionPanel> {
        let mut panels = BTreeMap::new();
        if let Ok(entries) = self.components.lock() {
            for ((owner, id), (spec, visible)) in entries.iter() {
                if *visible {
                    panels.insert(
                        format!(
                            "component:{:010}:{owner}:{id}",
                            (spec.order as i64) - (i32::MIN as i64)
                        ),
                        spec.clone(),
                    );
                }
            }
        }
        panels
    }

    /// Render visible footer entries, omitting keys consumed by richer UI.
    pub fn text_except(&self, keys: &[&str]) -> String {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(key, value)| {
                        !keys.contains(&key.as_str()) && !value.trim().is_empty()
                    })
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .unwrap_or_default()
    }

    /// The status line: non-empty values joined by two spaces, in key order.
    pub fn text(&self) -> String {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .values()
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_highlights_are_validated_atomic_and_cleared_on_plain_updates() {
        let m = ExtensionStatusMailbox::new();
        m.component(1, serde_json::json!({"op":"register","id":"sample"}))
            .unwrap();
        let update = serde_json::json!({"op":"update","id":"sample","text":"Err1",
            "highlights":[{"text":"Err1","color":"error"}]});
        m.component(1, update.clone()).unwrap();
        let before = m.panels();
        for highlights in [
            serde_json::json!([{"text":"", "color":"error"}]),
            serde_json::json!([{"text":"\u{1b}[31m", "color":"error"}]),
            serde_json::json!([{"text":"Err1", "color":"unknown"}]),
            serde_json::json!(vec![serde_json::json!({"text":"Err1","color":"error"}); 33]),
        ] {
            let mut invalid = update.clone();
            invalid["highlights"] = highlights;
            assert!(m.component(1, invalid).is_err());
            assert_eq!(m.panels(), before);
        }
        m.component(
            1,
            serde_json::json!({"op":"update","id":"sample","text":"OK"}),
        )
        .unwrap();
        assert!(m.panels().values().next().unwrap().highlights.is_empty());
    }

    #[test]
    fn components_keep_layout_content_visibility_and_owners_independent() {
        let m = ExtensionStatusMailbox::new();
        let register =
            serde_json::json!({"op":"register","id":"stats","slot":"footer.after","visible":false});
        m.component(1, register.clone()).unwrap();
        m.component(2, register.clone()).unwrap();
        assert!(m.panels().is_empty());
        m.component(
            1,
            serde_json::json!({"op":"update","id":"stats","text":"TPS 252"}),
        )
        .unwrap();
        m.component(1, serde_json::json!({"op":"show","id":"stats"}))
            .unwrap();
        let rev = m.revision();
        m.component(1, register).unwrap();
        assert_eq!(m.revision(), rev);
        assert_eq!(
            m.panels()["component:2147483648:1:stats"].layout,
            crate::PanelLayout::Footer
        );
        assert!(m
            .component(
                1,
                serde_json::json!({"op":"update","id":"stats","slot":"overlay"})
            )
            .is_err());
        assert!(m
            .component(
                3,
                serde_json::json!({"op":"update","id":"stats","text":"x"})
            )
            .is_err());
        m.component(1, serde_json::json!({"op":"hide","id":"stats"}))
            .unwrap();
        m.component(
            1,
            serde_json::json!({"op":"configure","id":"stats","slot":"sidebar.left"}),
        )
        .unwrap();
        m.component(1, serde_json::json!({"op":"show","id":"stats"}))
            .unwrap();
        assert_eq!(
            m.panels()["component:2147483648:1:stats"].lines,
            ["TPS 252"]
        );
        m.remove_owner(1);
        assert!(m.panels().is_empty());
        m.component(2, serde_json::json!({"op":"show","id":"stats"}))
            .unwrap();
        assert_eq!(m.panels().len(), 1);
    }
    #[test]
    fn set_and_clear_round_trip() {
        let mailbox = ExtensionStatusMailbox::new();
        assert_eq!(mailbox.text(), "");
        assert!(mailbox.set("langfuse", "langfuse ✓"));
        assert_eq!(mailbox.text(), "langfuse ✓");
        // Re-setting the same value is not a change (no repaint).
        assert!(!mailbox.set("langfuse", "langfuse ✓"));
        assert!(mailbox.set("langfuse", "langfuse ✓ (trace sent)"));
        assert_eq!(mailbox.text(), "langfuse ✓ (trace sent)");
        let before = mailbox.revision();
        // Clearing removes the entry and bumps the revision.
        assert!(mailbox.set("langfuse", ""));
        assert_eq!(mailbox.text(), "");
        assert!(mailbox.revision() > before);
        // Clearing an absent key is a no-op.
        assert!(!mailbox.set("langfuse", ""));
        assert!(!mailbox.set("  ", "ignored"));
    }

    #[test]
    fn multiple_extensions_render_in_key_order() {
        let mailbox = ExtensionStatusMailbox::new();
        mailbox.set("zeta", "z");
        mailbox.set("alpha", "a");
        assert_eq!(mailbox.text(), "a  z");
    }

    #[test]
    fn handle_parses_the_action_payload() {
        let mailbox = ExtensionStatusMailbox::new();
        let out = mailbox
            .handle(serde_json::json!({"key": "langfuse", "value": "✓"}))
            .expect("ok");
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(mailbox.text(), "✓");
        assert!(mailbox.handle(serde_json::json!({"value": "x"})).is_err());
    }
}
