//! Active progress checkpoint projection. Publication remains the caller's job.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Runs whose terminal records the host has acknowledged, keyed by lane.
pub type CompletedProgressRuns = HashMap<String, HashSet<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressOp {
    Append,
    ClearStream,
    ClearRun,
    /// A history binding, never legal in the independent progress store.
    CommitStream,
}

/// Metadata needed by the reducer; payloads remain opaque to this crate.
pub trait ProgressRecord: Clone {
    fn id(&self) -> &str;
    fn lane(&self) -> &str;
    fn run_id(&self) -> &str;
    fn stream_index(&self) -> u64;
    fn sequence(&self) -> u64;
    fn op(&self) -> ProgressOp;
    fn has_payload(&self) -> bool;
    fn stamp(&mut self, sequence: u64, timestamp: i64);
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProgressError {
    #[error("{0}")]
    InvalidPayload(&'static str),
    #[error("{0}")]
    InvalidCheckpoint(&'static str),
    #[error("{0}")]
    SequenceOverflow(&'static str),
}

type Result<T> = std::result::Result<T, ProgressError>;

/// The serialized shape deliberately matches the host's existing checkpoint.
/// After deserialization callers MUST validate before accepting any mutations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressState<F> {
    pub sequence: u64,
    pub frames: Vec<F>,
    #[serde(skip)]
    ids: HashSet<String>,
}

impl<F> Default for ProgressState<F> {
    fn default() -> Self {
        Self {
            sequence: 0,
            frames: Vec::new(),
            ids: HashSet::new(),
        }
    }
}

impl<F: ProgressRecord> ProgressState<F> {
    pub fn at_sequence(sequence: u64) -> Self {
        Self {
            sequence,
            ..Default::default()
        }
    }

    /// Validate a complete batch before stamping; no accumulated payload clones.
    pub fn prepare(&self, records: Vec<F>, mut now: impl FnMut() -> i64) -> Result<Vec<F>> {
        let mut ids = HashSet::with_capacity(records.len());
        for record in &records {
            if record.id().is_empty() || record.lane().is_empty() || record.run_id().is_empty() {
                return Err(ProgressError::InvalidPayload(
                    "progress identity must not be empty",
                ));
            }
            if self.ids.contains(record.id()) || !ids.insert(record.id()) {
                return Err(ProgressError::InvalidPayload("duplicate progress id"));
            }
            match record.op() {
                ProgressOp::Append if record.has_payload() => {}
                ProgressOp::ClearRun | ProgressOp::ClearStream if !record.has_payload() => {}
                _ => {
                    return Err(ProgressError::InvalidPayload(
                        "invalid progress operation; bindings belong to history",
                    ))
                }
            }
        }
        let mut sequence = self.sequence;
        records
            .into_iter()
            .map(|mut record| {
                sequence = sequence
                    .checked_add(1)
                    .ok_or(ProgressError::SequenceOverflow(
                        "progress sequence overflow",
                    ))?;
                record.stamp(sequence, now());
                Ok(record)
            })
            .collect()
    }

    /// Apply only a batch returned by prepare, after its append is acknowledged.
    pub fn apply(&mut self, records: &[F]) {
        for record in records {
            self.sequence = record.sequence();
            match record.op() {
                ProgressOp::Append => {
                    self.ids.insert(record.id().to_owned());
                    self.frames.push(record.clone());
                }
                ProgressOp::ClearStream | ProgressOp::ClearRun => {
                    let ids = &mut self.ids;
                    self.frames.retain(|frame| {
                        let keep = !clears(record, frame);
                        if !keep {
                            ids.remove(frame.id());
                        }
                        keep
                    });
                    self.shrink();
                }
                ProgressOp::CommitStream => unreachable!("validated progress batch"),
            }
        }
    }

    /// Candidate for publication; never clones payloads retired by this batch.
    pub fn project(&self, records: &[F]) -> Self {
        let frames: Vec<_> = self
            .frames
            .iter()
            .filter(|frame| !records.iter().any(|record| clears(record, frame)))
            .cloned()
            .collect();
        let ids = frames.iter().map(|frame| frame.id().to_owned()).collect();
        let mut projected = Self {
            sequence: self.sequence,
            frames,
            ids,
        };
        projected.apply(records);
        projected
    }

    pub fn prune_completed(&mut self, completed: &CompletedProgressRuns) -> usize {
        let before = self.frames.len();
        let ids = &mut self.ids;
        self.frames.retain(|frame| {
            if is_completed(frame, completed) {
                ids.remove(frame.id());
                false
            } else {
                true
            }
        });
        self.shrink();
        before - self.frames.len()
    }

    /// No candidate or payload clones when there is nothing to reclaim.
    pub fn project_completed(&self, completed: &CompletedProgressRuns) -> Option<Self> {
        if !self
            .frames
            .iter()
            .any(|frame| is_completed(frame, completed))
        {
            return None;
        }
        let frames: Vec<_> = self
            .frames
            .iter()
            .filter(|frame| !is_completed(*frame, completed))
            .cloned()
            .collect();
        let ids = frames.iter().map(|frame| frame.id().to_owned()).collect();
        Some(Self {
            sequence: self.sequence,
            frames,
            ids,
        })
    }

    pub fn read(&self, run_id: &str) -> Vec<F> {
        self.frames
            .iter()
            .filter(|frame| frame.run_id() == run_id)
            .cloned()
            .collect()
    }

    pub fn validate(&mut self) -> Result<()> {
        let mut previous = 0;
        let mut ids = HashSet::new();
        for frame in &self.frames {
            if frame.op() != ProgressOp::Append
                || !frame.has_payload()
                || frame.sequence() <= previous
                || frame.sequence() > self.sequence
                || frame.id().is_empty()
                || frame.lane().is_empty()
                || frame.run_id().is_empty()
                || !ids.insert(frame.id())
            {
                return Err(ProgressError::InvalidCheckpoint(
                    "invalid progress checkpoint",
                ));
            }
            previous = frame.sequence();
        }
        self.ids = ids.into_iter().map(str::to_owned).collect();
        Ok(())
    }

    fn shrink(&mut self) {
        if self.frames.capacity() > self.frames.len().saturating_mul(2) {
            self.frames.shrink_to_fit();
        }
        if self.ids.capacity() > self.ids.len().saturating_mul(2) {
            self.ids.shrink_to_fit();
        }
    }
}

fn clears<F: ProgressRecord>(record: &F, frame: &F) -> bool {
    frame.run_id() == record.run_id()
        && frame.lane() == record.lane()
        && (record.op() == ProgressOp::ClearRun
            || (record.op() == ProgressOp::ClearStream
                && frame.stream_index() == record.stream_index()))
}

fn is_completed<F: ProgressRecord>(frame: &F, completed: &CompletedProgressRuns) -> bool {
    completed
        .get(frame.lane())
        .is_some_and(|runs| runs.contains(frame.run_id()))
}

#[cfg(test)]
mod tests;
