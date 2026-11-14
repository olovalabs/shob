use super::*;
use crate::session::types::{AssistantFrameOp, AssistantFrameRecord, RecordBase, RecordPageQuery};

#[test]
fn pages_match_full_scan_for_lanes_gaps_bounds_and_both_directions() {
    let mut state = SessionState::new();
    state
        .apply_mutation(SessionMutation::Lane {
            seq: 1,
            lane: "other".into(),
            leaf_id: None,
        })
        .unwrap();
    for index in 1..20 {
        state.resync_sequence(index * 2);
        state
            .apply_mutation(SessionMutation::Record {
                record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                    base: RecordBase {
                        id: format!("r{index}"),
                        seq: index * 2 + 1,
                        lane: if index % 2 == 0 { "main" } else { "other" }.into(),
                        timestamp: 0,
                    },
                    run_id: "run".into(),
                    stream_index: index as usize,
                    op: AssistantFrameOp::Append,
                    frame: Some(serde_json::json!({"text":"x".repeat(4096)})),
                }),
            })
            .unwrap();
    }
    for lane in ["main", "other", "absent"] {
        for after_seq in [None, Some(0), Some(5), Some(16), Some(100)] {
            for before_seq in [None, Some(0), Some(5), Some(16), Some(100)] {
                for from_end in [false, true] {
                    for limit in [1, 3, 100] {
                        let mut expected = state
                            .find_records(&RecordQuery {
                                lane: Some(lane.into()),
                                after_seq,
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .unwrap();
                        expected.retain(|record| before_seq.is_none_or(|seq| record.seq() < seq));
                        let total = expected.len();
                        if from_end && expected.len() > limit {
                            expected = expected.split_off(expected.len() - limit);
                        } else {
                            expected.truncate(limit);
                        }
                        let actual = state
                            .record_page(&RecordPageQuery {
                                lane: lane.into(),
                                after_seq,
                                before_seq,
                                limit,
                                from_end,
                            })
                            .unwrap();
                        assert_eq!(actual.total, total);
                        assert_eq!(actual.records, expected);
                    }
                }
            }
        }
    }
    assert!(state
        .record_page(&RecordPageQuery {
            lane: "main".into(),
            after_seq: None,
            before_seq: None,
            limit: 0,
            from_end: false,
        })
        .is_err());
}
