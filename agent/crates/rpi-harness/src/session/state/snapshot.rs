//! Indexed SQLite snapshots: each entry/record payload is serialized once.
//! Derived lookup maps are rebuilt and log references validated on load.
use super::*;
use serde::ser::SerializeStruct;

const FORMAT: &str = "indexed-session-v1";

impl Serialize for SessionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SessionState", 10)?;
        state.serialize_field("format", FORMAT)?;
        state.serialize_field("sequence", &self.index.sequence)?;
        state.serialize_field("entries", &self.index.entries)?;
        state.serialize_field("records", &self.index.records)?;
        state.serialize_field("lanes", &self.index.lanes)?;
        state.serialize_field("log", &self.index.log)?;
        state.serialize_field("stats", &self.index.stats)?;
        state.serialize_field("name", &self.index.name)?;
        state.serialize_field("labels", &self.index.labels)?;
        // Retired frame bodies are gone, but their IDs must remain taken after
        // every SQLite snapshot refresh, not only during JSONL replay.
        let live_ids: std::collections::HashSet<_> = self
            .index
            .entries
            .iter()
            .map(Entry::id)
            .chain(self.index.records.iter().map(LaneRecord::id))
            .collect();
        let mut reserved_ids: Vec<_> = self
            .index
            .used_ids
            .iter()
            .filter(|id| !live_ids.contains(id.as_str()))
            .collect();
        reserved_ids.sort_unstable();
        state.serialize_field("reserved_ids", &reserved_ids)?;
        state.end()
    }
}

#[derive(Deserialize)]
struct Snapshot {
    format: String,
    sequence: u64,
    entries: Vec<Entry>,
    records: Vec<LaneRecord>,
    lanes: BTreeMap<String, Option<String>>,
    log: Vec<StoredLogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
    #[serde(default)]
    reserved_ids: Vec<String>,
}

impl<'de> Deserialize<'de> for SessionState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let snapshot = Snapshot::deserialize(deserializer)?;
        if snapshot.format != FORMAT {
            return Err(D::Error::custom("unsupported session snapshot format"));
        }
        let entries_by_id: HashMap<_, _> = snapshot
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id().to_owned(), index))
            .collect();
        if entries_by_id.len() != snapshot.entries.len() {
            return Err(D::Error::custom("duplicate snapshot entry id"));
        }
        let mut used_ids: std::collections::HashSet<_> = snapshot
            .entries
            .iter()
            .map(|entry| entry.id().to_owned())
            .chain(snapshot.records.iter().map(|record| record.id().to_owned()))
            .collect();
        if used_ids.len() != snapshot.entries.len() + snapshot.records.len() {
            return Err(D::Error::custom("duplicate snapshot payload id"));
        }
        for id in snapshot.reserved_ids {
            if !used_ids.insert(id) {
                return Err(D::Error::custom("duplicate snapshot reserved id"));
            }
        }
        let mut last_seq = 0;
        let mut entry_indexes = std::collections::HashSet::new();
        let mut record_indexes = std::collections::HashSet::new();
        for item in &snapshot.log {
            let seq = match item {
                StoredLogItem::Entry(index) => {
                    if *index != entry_indexes.len() || !entry_indexes.insert(*index) {
                        return Err(D::Error::custom("invalid snapshot entry reference order"));
                    }
                    snapshot.entries.get(*index).map(Entry::seq)
                }
                StoredLogItem::Record(index) => {
                    if *index != record_indexes.len() || !record_indexes.insert(*index) {
                        return Err(D::Error::custom("invalid snapshot record reference order"));
                    }
                    snapshot.records.get(*index).map(LaneRecord::seq)
                }
                StoredLogItem::Other(item) => {
                    if matches!(
                        item.as_ref(),
                        LogItem::Entry { .. } | LogItem::Record { .. }
                    ) {
                        return Err(D::Error::custom("snapshot payload must use an index"));
                    }
                    Some(item.seq())
                }
            }
            .ok_or_else(|| D::Error::custom("snapshot log index is out of bounds"))?;
            if seq <= last_seq || seq > snapshot.sequence {
                return Err(D::Error::custom("invalid snapshot log sequence"));
            }
            last_seq = seq;
        }
        if entry_indexes.len() != snapshot.entries.len()
            || record_indexes.len() != snapshot.records.len()
        {
            return Err(D::Error::custom("snapshot contains unreferenced payloads"));
        }
        // Derive identity only from the authoritative ordered lane mutations;
        // no new persisted generation field or migration is needed.
        let mut lane_generations: BTreeMap<String, u64> = snapshot
            .lanes
            .keys()
            .map(|lane| (lane.clone(), 0))
            .collect();
        for item in &snapshot.log {
            if let StoredLogItem::Other(item) = item {
                if let LogItem::Lane { seq, lane, .. } = item.as_ref() {
                    if !snapshot.lanes.contains_key(lane) {
                        return Err(D::Error::custom("snapshot lane mutation has no lane"));
                    }
                    lane_generations.insert(lane.clone(), *seq);
                }
            }
        }
        let mut open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>> =
            HashMap::new();
        for record in &snapshot.records {
            match record {
                LaneRecord::OperationStarted(started) => {
                    open_operations_by_lane
                        .entry(started.base.lane.clone())
                        .or_default()
                        .insert(started.base.seq, started.clone());
                }
                LaneRecord::OperationFinished(finished) => {
                    if let Some(open) = open_operations_by_lane.get_mut(&finished.base.lane) {
                        open.retain(|_, operation| operation.base.id != finished.run_id);
                    }
                }
                _ => {}
            }
        }
        // Rebuild the derived role index. Reject malformed parent graphs rather
        // than allowing an optimized ancestry walk to loop or cross a branch.
        if entries_by_id.len() != snapshot.entries.len() {
            return Err(D::Error::custom("snapshot has duplicate entry IDs"));
        }
        let mut nearest_user = Vec::with_capacity(snapshot.entries.len());
        for (index, entry) in snapshot.entries.iter().enumerate() {
            let previous = match entry.parent_id() {
                Some(id) => {
                    let parent = entries_by_id
                        .get(id)
                        .filter(|parent| **parent < index)
                        .ok_or_else(|| {
                            D::Error::custom("snapshot has a missing or forward parent")
                        })?;
                    nearest_user[*parent]
                }
                None => None,
            };
            nearest_user.push(
                if matches!(
                    entry.as_message(),
                    Some(rpi_agent::message::AgentMessage::User(_))
                ) {
                    Some(index)
                } else {
                    previous
                },
            );
        }
        let metadata = snapshot.entries.iter().map(EntryMeta::from_entry).collect();
        let hot_append_count = snapshot
            .records
            .iter()
            .filter(|record| {
                matches!(record, LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::Append)
            })
            .count();
        let mut state = Self {
            index: IndexedState {
                metadata,
                nearest_user,
                sequence: snapshot.sequence,
                used_ids,
                entries: snapshot.entries,
                entries_by_id,
                records: snapshot.records,
                hot_append_count,
                #[cfg(test)]
                retire_record_checks: 0,
                open_operations_by_lane,
                lanes: snapshot.lanes,
                lane_generations,
                log: snapshot.log,
                stats: snapshot.stats,
                name: snapshot.name,
                labels: snapshot.labels,
            },
        };
        // Old snapshots can still contain append bodies covered by a clear.
        // Validate every original reference first, then retire bodies and remap
        // their log indexes together. IDs remain permanently reserved.
        let clears: Vec<_> = state
            .index
            .records
            .iter()
            .filter_map(|record| match record {
                LaneRecord::AssistantFrame(frame)
                    if matches!(
                        frame.op,
                        AssistantFrameOp::ClearStream | AssistantFrameOp::ClearRun
                    ) =>
                {
                    Some(frame.clone())
                }
                _ => None,
            })
            .collect();
        for frame in clears {
            state.index.retire_cleared_frames(&frame);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{
        AssistantFrameOp, AssistantFrameRecord, EntryBase, MessageEntry, RecordBase,
    };

    fn state() -> SessionState {
        let mut state = SessionState::new();
        for index in 0..2 {
            let entry = Entry::Message(MessageEntry {
                base: EntryBase {
                    entry_type: "message".into(),
                    id: format!("e{index}"),
                    seq: index + 1,
                    parent_id: (index > 0).then(|| "e0".into()),
                    timestamp: 42,
                },
                message: rpi_ai::UserMessage::new(format!("unique payload {index} 🐈"), 42).into(),
                terminate: None,
            });
            state
                .apply_mutation(SessionMutation::Entry {
                    seq: index + 1,
                    timestamp: 42,
                    lane: Some("main".into()),
                    entry,
                })
                .unwrap();
        }
        state
            .apply_mutation(SessionMutation::Record {
                record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                    base: RecordBase {
                        id: "frame".into(),
                        seq: 3,
                        lane: "main".into(),
                        timestamp: 42,
                    },
                    run_id: "run".into(),
                    stream_index: 0,
                    op: AssistantFrameOp::ClearRun,
                    frame: None,
                }),
            })
            .unwrap();
        state
            .apply_mutation(SessionMutation::FactName {
                seq: 4,
                name: Some("name".into()),
            })
            .unwrap();
        state
    }

    #[test]
    fn indexed_snapshot_stores_payload_once_and_roundtrips_defensive_reads() {
        let state = state();
        let encoded = serde_json::to_string(&state).unwrap();
        assert_eq!(encoded.matches("unique payload 0").count(), 1);
        assert!(!encoded.contains("entries_by_id"));
        assert!(!encoded.contains("open_operations_by_lane"));
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["log"][0]["Entry"], 0);
        assert_eq!(value["log"][2]["Record"], 0);
        let restored: SessionState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            restored.get_log(&LogOptions::default()).unwrap(),
            state.get_log(&LogOptions::default()).unwrap()
        );
        assert_eq!(
            restored
                .get_log(&LogOptions {
                    after_seq: Some(2),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            2
        );
        let mut entry = restored.get_entry("e0").unwrap().clone();
        entry.base_mut().timestamp = 999;
        assert_eq!(restored.get_entry("e0").unwrap().base().timestamp, 42);
    }

    #[test]
    fn invalid_snapshot_indexes_sequences_and_formats_are_rejected() {
        for (field, replacement) in [
            ("log", serde_json::json!([{"Entry": 99}])),
            ("log", serde_json::json!([{"Entry": 0}, {"Entry": 0}])),
            ("sequence", serde_json::json!(0)),
            ("format", serde_json::json!("unknown")),
        ] {
            let mut value = serde_json::to_value(state()).unwrap();
            value[field] = replacement;
            assert!(serde_json::from_value::<SessionState>(value).is_err());
        }
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::session::types::{AssistantFrameRecord, RecordBase};
    #[test]
    fn old_snapshot_retires_only_prior_bodies_and_remaps_record_log_atomically() {
        let records: Vec<_> = [
            (1, AssistantFrameOp::Append),
            (2, AssistantFrameOp::ClearRun),
            (3, AssistantFrameOp::Append),
        ]
        .into_iter()
        .map(|(seq, op)| {
            LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id: format!("frame-{seq}"),
                    seq,
                    lane: "main".into(),
                    timestamp: 1,
                },
                run_id: "run".into(),
                stream_index: 0,
                op,
                frame: Some(serde_json::json!({"body":"stale or live"})),
            })
        })
        .collect();
        let mut value = serde_json::to_value(SessionState::new()).unwrap();
        value.as_object_mut().unwrap().remove("reserved_ids"); // Legacy snapshot.
        value["sequence"] = serde_json::json!(3);
        value["records"] = serde_json::to_value(&records).unwrap();
        value["log"] = serde_json::json!([{"Record":0},{"Record":1},{"Record":2}]);
        let restored: SessionState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.index.records, records[1..]);
        assert!(restored.validate_unused_id("frame-1").is_err());
        assert_eq!(restored.index.log.len(), 2);
        assert!(matches!(restored.index.log[0], StoredLogItem::Record(0)));
        assert!(matches!(restored.index.log[1], StoredLogItem::Record(1)));
        let encoded = serde_json::to_value(&restored).unwrap();
        assert_eq!(encoded["records"].as_array().unwrap().len(), 2);
        assert_eq!(encoded["reserved_ids"], serde_json::json!(["frame-1"]));
        let mut restored = restored;
        for _ in 0..3 {
            restored = serde_json::from_value(serde_json::to_value(&restored).unwrap()).unwrap();
            assert!(restored.validate_unused_id("frame-1").is_err());
            assert_eq!(restored.index.records, records[1..]);
            assert_eq!(restored.index.log.len(), 2);
        }
        for ids in [
            serde_json::json!(["frame-1", "frame-1"]),
            serde_json::json!(["frame-2"]),
        ] {
            let mut corrupt = encoded.clone();
            corrupt["reserved_ids"] = ids;
            assert!(serde_json::from_value::<SessionState>(corrupt).is_err());
        }
    }
}
