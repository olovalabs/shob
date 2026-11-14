//! Session-scoped key/value facts.
//!
//! Mirrors `packages/coding-agent/src/core/session/values.ts`. Values are
//! durable, latest-wins, and survive recovery because they are persisted as
//! ordinary custom entries with a reserved `customType` — no new storage
//! primitive is needed, so every backend (JSONL / in-memory / SQLite) supports
//! them for free, and the append-only invariants are preserved.
//!
//! Shape of a value entry:
//!
//! ```jsonc
//! { "type": "custom", "customType": "rpi.value", "data": { "key": "…", "value": … } }
//! ```
//!
//! A `null` `value` is a tombstone: the key is treated as deleted from that
//! entry forward. Reads scan oldest→newest and keep the last write per key, so
//! a deletion only shadows writes that precede it.
//!
//! Ordering matters: the store must be read with `EntryOrder::OldestFirst` or
//! the last-write resolution is wrong. [`SessionValues::load`] always asks for
//! oldest-first, so callers cannot get this wrong.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{SessionError, SessionResult};
use crate::session::session::Session;
use crate::session::types::{Entry, EntryOrder, EntryQuery};

/// The reserved `customType` that marks a session value entry.
pub const VALUE_CUSTOM_TYPE: &str = "rpi.value";

/// A loaded, latest-wins view of the session's values. Cheap to clone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionValues {
    values: BTreeMap<String, Value>,
}

impl SessionValues {
    /// Load every value from the session, resolving latest-wins and dropping
    /// tombstoned keys.
    pub async fn load(session: &Session) -> SessionResult<Self> {
        let entries = session
            .find_entries(&EntryQuery {
                entry_type: Some("custom"),
                custom_type: Some(VALUE_CUSTOM_TYPE.to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await?;
        Ok(Self::from_entries(&entries))
    }

    /// Resolve a value set from already-fetched entries (oldest first).
    pub fn from_entries(entries: &[Entry]) -> Self {
        let mut values = BTreeMap::new();
        for entry in entries {
            let Entry::Custom(custom) = entry else {
                continue;
            };
            if custom.custom_type != VALUE_CUSTOM_TYPE {
                continue;
            }
            let Some(data) = &custom.data else {
                continue;
            };
            let Some(key) = data.get("key").and_then(Value::as_str) else {
                continue;
            };
            match data.get("value") {
                // Tombstone: delete from this point forward.
                Some(Value::Null) | None => {
                    values.remove(key);
                }
                Some(value) => {
                    values.insert(key.to_string(), value.clone());
                }
            }
        }
        Self { values }
    }

    /// Read a value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Read a typed value, returning `None` when absent or the wrong shape.
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let value = self.values.get(key)?;
        serde_json::from_value(value.clone()).ok()
    }

    /// Every live key/value pair, ordered by key.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The number of live keys.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no live keys.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Durable writer for session values. Each write appends one custom entry, so
/// the operation is crash-safe and ordering-preserving.
#[derive(Clone)]
pub struct SessionValueWriter {
    session: Session,
}

impl SessionValueWriter {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// `setValue(key, value)` — persist a value (latest-wins).
    pub async fn set(&self, key: &str, value: Value) -> SessionResult<String> {
        self.append(key, Some(value)).await
    }

    /// `deleteValue(key)` — persist a tombstone.
    pub async fn delete(&self, key: &str) -> SessionResult<String> {
        self.append(key, None).await
    }

    async fn append(&self, key: &str, value: Option<Value>) -> SessionResult<String> {
        if key.trim().is_empty() {
            return Err(SessionError::invalid_payload(
                "session value key must not be empty",
            ));
        }
        let data = serde_json::json!({
            "key": key,
            "value": value.unwrap_or(Value::Null),
        });
        self.session
            .append_custom_entry(VALUE_CUSTOM_TYPE, Some(data))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::session::{DefaultIdGenerator, Session};
    use crate::session::types::SessionMetadata;
    use std::sync::Arc;

    fn session() -> Session {
        let metadata = SessionMetadata {
            id: "s-values".to_string(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        Session::new(Arc::new(storage), None)
    }

    #[tokio::test]
    async fn set_then_load_round_trips_and_latest_wins() {
        let session = session();
        let writer = SessionValueWriter::new(session.clone());
        writer.set("a", serde_json::json!(1)).await.unwrap();
        writer.set("b", serde_json::json!("two")).await.unwrap();
        writer.set("a", serde_json::json!(3)).await.unwrap();

        let values = SessionValues::load(&session).await.unwrap();
        assert_eq!(values.get("a"), Some(&serde_json::json!(3)));
        assert_eq!(values.get("b"), Some(&serde_json::json!("two")));
        assert_eq!(values.len(), 2);
    }

    #[tokio::test]
    async fn delete_tombstones_a_key() {
        let session = session();
        let writer = SessionValueWriter::new(session.clone());
        writer.set("k", serde_json::json!(true)).await.unwrap();
        writer.delete("k").await.unwrap();

        let values = SessionValues::load(&session).await.unwrap();
        assert!(values.is_empty());
        assert!(values.get("k").is_none());
    }

    #[tokio::test]
    async fn typed_reads_decode_structs() {
        let session = session();
        let writer = SessionValueWriter::new(session.clone());
        writer
            .set("cfg", serde_json::json!({ "n": 7, "s": "x" }))
            .await
            .unwrap();

        #[derive(serde::Deserialize)]
        struct Cfg {
            n: i64,
            s: String,
        }
        let values = SessionValues::load(&session).await.unwrap();
        let cfg: Cfg = values.get_as("cfg").unwrap();
        assert_eq!(cfg.n, 7);
        assert_eq!(cfg.s, "x");
    }

    #[tokio::test]
    async fn empty_key_is_rejected() {
        let session = session();
        let writer = SessionValueWriter::new(session);
        assert!(writer.set("  ", serde_json::json!(1)).await.is_err());
    }
}
