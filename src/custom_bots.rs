use crate::bot_avatar::AvatarConfig;
use gpui::SharedString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Bot {
    pub id: String,
    #[serde(serialize_with = "serialize_name", deserialize_with = "deserialize_name")]
    pub name: SharedString,
    pub avatar: AvatarConfig,
    #[serde(serialize_with = "serialize_prompt", deserialize_with = "deserialize_prompt")]
    pub system_prompt: Arc<str>,
    pub created_at: u64,
}

fn serialize_name<S: Serializer>(name: &SharedString, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(name.as_str())
}

fn deserialize_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SharedString, D::Error> {
    String::deserialize(deserializer).map(SharedString::from)
}

fn serialize_prompt<S: Serializer>(prompt: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(prompt)
}

fn deserialize_prompt<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Arc<str>, D::Error> {
    String::deserialize(deserializer).map(Arc::from)
}
