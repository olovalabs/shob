//! Invocation-local output hygiene, not a sandbox for native plugins. Match
//! values supplied by this host, never business property names or bare handles.
use super::*;

const REDACTED: &str = "[redacted invocation credential]";

#[derive(Clone, Default)]
pub(super) struct PublicBindingGuard {
    metadata: Option<Value>,
    key: Option<String>,
}

impl PublicBindingGuard {
    /// Call only after host injection, not on untrusted model parameters. The
    /// callback context owns this snapshot through the final foreign cleanup.
    pub(super) fn capture(params: &Value, invocation: Option<&crate::ActionBridge>) -> Self {
        let Some(key) = invocation.and_then(crate::ActionBridge::invocation_key) else {
            return Self::default();
        };
        let metadata = params.get(rpi_plugin_sdk::HOST_CONTEXT_KEY);
        if metadata
            .and_then(|v| v.get("runtimeKey"))
            .and_then(Value::as_str)
            != Some(key)
        {
            return Self::default();
        }
        Self {
            metadata: metadata.cloned(),
            key: Some(key.to_owned()),
        }
    }

    pub(super) fn string(&self, text: &str) -> String {
        let Some(key) = &self.key else {
            return text.to_owned();
        };
        // Decode ordinary serialized JSON as well: metadata can be echoed in a
        // text block, and renaming secret-bearing keys must detect collisions.
        if let Ok(mut value @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(text) {
            let original = value.clone();
            self.value(&mut value);
            if value != original {
                return value.to_string();
            }
        }
        text.replace(key, REDACTED)
    }

    fn value(&self, value: &mut Value) {
        if self.key.is_none() {
            return;
        }
        if self.metadata.as_ref() == Some(value) {
            *value = Value::Null;
            return;
        }
        match value {
            Value::String(text) => *text = self.string(text),
            Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            Value::Object(object) => {
                let mut public = serde_json::Map::new();
                for (key, mut item) in std::mem::take(object) {
                    let key = self.string(&key);
                    self.value(&mut item);
                    if public.insert(key, item).is_some() {
                        // Do not silently overwrite a sibling after redaction.
                        *value = Value::String(REDACTED.into());
                        return;
                    }
                }
                *object = public;
            }
            _ => {} // Numeric handles alone are not credentials.
        }
    }

    pub(super) fn result(&self, mut result: AgentToolResult) -> AgentToolResult {
        if self.key.is_none() {
            return result;
        }
        for block in &mut result.content {
            match block {
                TextContentOrImage::Text(text) => {
                    text.text = self.string(&text.text);
                    if let Some(signature) = &mut text.text_signature {
                        *signature = self.string(signature);
                    }
                }
                TextContentOrImage::Image(image) => {
                    image.data = self.string(&image.data).into();
                    image.mime_type = self.string(&image.mime_type);
                }
            }
        }
        self.value(&mut result.details);
        for name in &mut result.added_tool_names {
            *name = self.string(name);
        }
        // Leave usage, terminate and the outer result structure intact.
        result
    }

    pub(super) fn terminal(
        &self,
        result: Result<AgentToolResult, AgentError>,
    ) -> Result<AgentToolResult, AgentError> {
        result
            .map(|result| self.result(result))
            .map_err(|error| match error {
                AgentError::StructuredTool(result) => {
                    AgentError::StructuredTool(Box::new(self.result(*result)))
                }
                AgentError::Tool(text) => AgentError::Tool(self.string(&text)),
                other => other,
            })
    }
}
