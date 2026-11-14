//! Transport-independent dialog semantics. Presentation kinds do not imply permission.
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub title: String,
    pub permission: String,
}

/// Validate the explicit permission semantics and retain the plugin's answer labels.
pub fn permission_options(ui: &Value) -> Result<Vec<PermissionOption>, String> {
    if ui.get("purpose").and_then(Value::as_str) != Some("permission") {
        return Err("dialog is not a permission request".to_owned());
    }
    if !matches!(
        ui.get("kind").and_then(Value::as_str),
        Some("confirm" | "selector")
    ) || [
        "allowMultiple",
        "allow_multiple",
        "multiple",
        "allowFreeform",
        "allow_freeform",
        "allow_custom",
    ]
    .iter()
    .any(|key| ui.get(*key).and_then(Value::as_bool) == Some(true))
        || ui.get("questions").is_some()
    {
        return Err("permission dialogs require a single choice without freeform input".to_owned());
    }
    let options = ui
        .get("options")
        .and_then(Value::as_array)
        .ok_or("permission dialog requires options")?;
    let mut result: Vec<PermissionOption> = Vec::new();
    for option in options {
        let title = option
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.trim() == *s)
            .ok_or("permission option requires a nonempty title")?;
        let permission = option
            .get("permission")
            .and_then(Value::as_str)
            .filter(|s| {
                matches!(
                    *s,
                    "allow_once" | "allow_always" | "reject_once" | "reject_always"
                )
            })
            .ok_or("permission option requires an explicit allow/reject decision")?;
        if result.iter().any(|choice| choice.title == title) {
            return Err("permission option titles must be unique".to_owned());
        }
        result.push(PermissionOption {
            title: title.to_owned(),
            permission: permission.to_owned(),
        });
    }
    if result.is_empty() {
        return Err("permission dialog requires at least one option".to_owned());
    }
    Ok(result)
}

pub fn validate(ui: &Value, tool_call_id: Option<&str>) -> Result<(), String> {
    match ui.get("purpose") {
        None => Ok(()),
        Some(Value::String(purpose)) if purpose == "question" => Ok(()),
        Some(Value::String(purpose)) if purpose == "permission" => {
            if tool_call_id.map_or(true, |id| id.trim().is_empty()) {
                return Err("permission dialog requires a nonempty toolCallId".to_owned());
            }
            permission_options(ui).map(|_| ())
        }
        _ => Err("dialog purpose must be question or permission".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selectors_are_questions_until_explicitly_declared_permissions() {
        let ui = json!({"kind":"selector","options":[{"title":"Rust"},{"title":"Python"}]});
        assert!(validate(&ui, None).is_ok());
        assert!(permission_options(&ui).is_err());
    }

    #[test]
    fn permission_options_preserve_labels_and_require_semantics() {
        let mut ui = json!({"purpose":"permission","kind":"confirm","options":[
            {"title":"允许一次","permission":"allow_once"},
            {"title":"拒绝","permission":"reject_once"}
        ]});
        assert!(validate(&ui, None).is_err());
        assert!(validate(&ui, Some("tool-1")).is_ok());
        assert_eq!(permission_options(&ui).unwrap()[0].title, "允许一次");
        ui["options"][1]["permission"] = json!("arbitrary");
        assert!(validate(&ui, Some("tool-1")).is_err());
    }
}
