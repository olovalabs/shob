//! Host adapter for the backend-independent active-progress checkpoint reducer.
use super::types::{AssistantFrameOp, AssistantFrameRecord};
use rpi_durable::progress::{ProgressOp, ProgressRecord};

pub(crate) use rpi_durable::progress::CompletedProgressRuns;
pub(crate) type ProgressState = rpi_durable::progress::ProgressState<AssistantFrameRecord>;

impl ProgressRecord for AssistantFrameRecord {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn lane(&self) -> &str {
        &self.base.lane
    }
    fn run_id(&self) -> &str {
        &self.run_id
    }
    fn stream_index(&self) -> u64 {
        self.stream_index as u64
    }
    fn sequence(&self) -> u64 {
        self.base.seq
    }
    fn has_payload(&self) -> bool {
        self.frame.is_some()
    }
    fn op(&self) -> ProgressOp {
        match self.op {
            AssistantFrameOp::Append => ProgressOp::Append,
            AssistantFrameOp::ClearStream => ProgressOp::ClearStream,
            AssistantFrameOp::ClearRun => ProgressOp::ClearRun,
            AssistantFrameOp::CommitStream => ProgressOp::CommitStream,
        }
    }
    fn stamp(&mut self, sequence: u64, timestamp: i64) {
        self.base.seq = sequence;
        self.base.timestamp = timestamp;
    }
}

impl From<rpi_durable::progress::ProgressError> for crate::error::SessionError {
    fn from(error: rpi_durable::progress::ProgressError) -> Self {
        use rpi_durable::progress::ProgressError;
        match error {
            ProgressError::InvalidPayload(message) => Self::invalid_payload(message),
            ProgressError::InvalidCheckpoint(message) => Self::invalid_entry(message),
            ProgressError::SequenceOverflow(message) => Self::storage(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{SessionError, SessionErrorCode};
    use rpi_durable::progress::ProgressError;
    use serde_json::json;

    #[test]
    fn existing_checkpoint_shape_and_error_codes_are_unchanged() {
        let checkpoint = json!({
            "sequence": 9,
            "frames": [{"id":"frame", "seq":7, "timestamp":17, "lane":"main",
                "runId":"run", "streamIndex":2, "op":"append", "frame":{"delta":"🌊"}}]
        });
        let mut restored: ProgressState = serde_json::from_value(checkpoint.clone()).unwrap();
        restored.validate().unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), checkpoint);
        assert_eq!(restored.read("run")[0].stream_index, 2);
        assert_eq!(
            SessionError::from(ProgressError::InvalidPayload("bad")).code,
            SessionErrorCode::InvalidPayload
        );
        assert_eq!(
            SessionError::from(ProgressError::InvalidCheckpoint("bad")).code,
            SessionErrorCode::InvalidEntry
        );
        assert_eq!(
            SessionError::from(ProgressError::SequenceOverflow("bad")).code,
            SessionErrorCode::Storage
        );
        let duplicate = restored.frames[0].clone();
        let error: SessionError = restored.prepare(vec![duplicate], || 0).unwrap_err().into();
        assert_eq!(error.code, SessionErrorCode::InvalidPayload);
        assert_eq!(error.message, "duplicate progress id");
    }
}
