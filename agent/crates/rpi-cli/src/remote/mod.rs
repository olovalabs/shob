//! Remote client support for `rpi --connect`.
//!
//! rpi is a Rust replacement for the TS `pi` CLI, not a wrapper around it, so
//! this module owns rpi's *own* wire contract instead of depending on any
//! external implementation. The design borrows the shape of the TS
//! `packages/coding-agent/src/modes/rpc/` layer (commands on stdin, responses +
//! events on stdout, a session abstraction the UI can render), but every type
//! here is native Rust.
//!
//! Pieces:
//! - [`protocol`] — the shared wire types ([`protocol::RemoteEvent`],
//!   [`protocol::RemoteCommand`], [`protocol::RemoteResponse`]) used by both the
//!   `--mode rpc` server and the `--connect` client, so the contract has a
//!   single source of truth.
//! - [`framing`] / [`cbor`] / [`codec`] — the **binary** transport: 4-byte
//!   length-prefixed frames carrying CBOR-encoded protocol messages (port of
//!   the reference JSONL transport remains
//!   the default; the binary one is available for callers that want it.
//! - [`client`] — the TCP JSON-RPC connection to a `rpi --server` endpoint.
//! - [`session`] — the client-side transcript + state the UI renders.
//!
//! The client itself (`--connect`) is built on top of these types: a TCP JSONL
//! connection, a remote session that folds the event stream into a transcript,
//! and a terminal UI that renders it. The client holds **no** local agent
//! resources — no provider, tools, extensions, or session files.

pub mod cbor;
pub mod client;
pub mod codec;
pub mod framing;
pub(crate) mod message_accumulator;
pub mod protocol;
pub mod session;
pub mod tui;
