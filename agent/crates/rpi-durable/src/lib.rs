//! Native persistence/recovery primitives, independent of storage and execution.
//!
//! The host owns append acknowledgement, branch identity and terminal decisions.
//! This crate never writes files, executes tools or decides that a run completed.

pub mod progress;
