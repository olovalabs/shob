use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};

#[derive(Clone)]
pub(crate) struct Field {
    pub label: String,
    pub kind: Kind,
    pub options: Vec<String>,
    pub required: bool,
    pub custom: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    Number,
    Toggle,
    Single,
    Multiple,
}

#[derive(Clone)]
pub(crate) enum Response {
    Submit(Value),
    AllowOnce,
    AllowSession,
    Deny,
}

pub(crate) struct Request {
    pub id: u64,
    pub title: String,
    pub detail: Option<String>,
    pub fields: Vec<Field>,
    pub approval: bool,
    pub allow_session: bool,
    pub reply: Sender<Response>,
}

impl Request {
    pub fn alive(&self) -> bool {
        !self.reply.is_closed()
    }
}

pub(crate) fn fields(args: &Value, questions: bool) -> Result<Vec<Field>, &'static str> {
    args[if questions { "questions" } else { "fields" }]
        .as_array()
        .ok_or("Missing input fields")?
        .iter()
        .map(|field| {
            let label = field["label"].as_str().ok_or("Missing field label")?.trim();
            let options: Vec<String> = field["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .collect();
            let kind = if questions {
                if options.is_empty() {
                    Kind::Text
                } else if field["multiple"] == true {
                    Kind::Multiple
                } else {
                    Kind::Single
                }
            } else {
                match field["type"].as_str() {
                    Some("text") => Kind::Text,
                    Some("number") => Kind::Number,
                    Some("toggle") => Kind::Toggle,
                    Some("select") => Kind::Single,
                    Some("multiselect") => Kind::Multiple,
                    _ => return Err("Unsupported field type"),
                }
            };
            if label.is_empty()
                || (matches!(kind, Kind::Single | Kind::Multiple) && options.is_empty())
            {
                return Err("A field needs a label and choice fields need options");
            }
            Ok(Field {
                label: label.into(),
                kind,
                options,
                required: questions || field["required"] == true,
                custom: questions,
            })
        })
        .collect()
}

pub(crate) fn request(
    title: String,
    detail: Option<String>,
    fields: Vec<Field>,
    approval: bool,
) -> (Request, Receiver<Response>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let (reply, receiver) = smol::channel::bounded(1);
    (
        Request {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            title,
            detail,
            fields,
            approval,
            allow_session: approval,
            reply,
        },
        receiver,
    )
}

pub(crate) fn field_value(field: &Field, text: &str, choices: &[usize]) -> Result<Value, String> {
    let text = text.trim();
    let mut picked = choices
        .iter()
        .filter_map(|&index| field.options.get(index))
        .cloned()
        .collect::<Vec<_>>();
    let answer = match field.kind {
        Kind::Toggle => return Ok(json!(!choices.is_empty())),
        Kind::Text if !text.is_empty() => json!(text),
        Kind::Number if !text.is_empty() => {
            let number = text
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .ok_or_else(|| format!("Enter a number for {}.", field.label))?;
            json!(number)
        }
        Kind::Single if field.custom && !text.is_empty() => json!(text),
        Kind::Single if !picked.is_empty() => json!(picked[0]),
        Kind::Multiple => {
            if field.custom && !text.is_empty() {
                picked.push(text.to_owned());
            }
            if picked.is_empty() {
                Value::Null
            } else {
                json!(picked)
            }
        }
        _ => Value::Null,
    };
    if answer.is_null() && field.required {
        return Err(format!("Answer {} before submitting.", field.label));
    }
    Ok(answer)
}

#[derive(Default)]
pub(crate) struct Draft {
    pub fields: Vec<FieldDraft>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct FieldDraft {
    pub input: Option<gpui::Entity<gpui_component::input::InputState>>,
    pub choices: Vec<usize>,
}
