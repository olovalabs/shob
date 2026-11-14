//! TCP JSON-RPC client for the `rpi --server` endpoint.
//!
//! Wire shape (one JSON object per line), spoken by `rpi-package`'s
//! `rpi-server` extension:
//!
//! ```text
//! -> {"jsonrpc":"2.0","id":1,"method":"start_session","params":{…}}
//! <- {"jsonrpc":"2.0","id":1,"result":{"sessionId":"session-1",…}}
//! -> {"jsonrpc":"2.0","id":2,"method":"subscribe","params":{"sessionId":"session-1"}}
//! <- {"jsonrpc":"2.0","id":2,"result":{"subscriptionId":7,…}}
//! -> {"jsonrpc":"2.0","id":3,"method":"send","params":{"sessionId":"session-1","command":{…}}}
//! <- {"jsonrpc":"2.0","method":"event","params":{"subscriptionId":7,"event":<child line>}}
//! ```
//!
//! The `<child line>` payloads are exactly the `--mode rpc` lines
//! ([`crate::remote::protocol::RemoteEvent`], responses, `ready`), so the client
//! never needs to know how the server spawned the agent.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A connected JSON-RPC client.
pub struct RemoteClient {
    reader: Option<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl RemoteClient {
    /// Connect to `addr` (e.g. `127.0.0.1:9899`).
    pub async fn connect(addr: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("could not connect to {addr}: {e}"))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: Some(BufReader::new(read_half)),
            writer: write_half,
            next_id: 1,
        })
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn write_value(&mut self, value: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("flush failed: {e}"))
    }

    /// Authenticate the connection. Required when the server was started with a
    /// token; a no-op when `token` is empty (server auth disabled).
    pub async fn authenticate(&mut self, token: &str) -> Result<(), String> {
        if token.is_empty() {
            return Ok(());
        }
        self.call("authenticate", json!({"token": token}))
            .await
            .map(|_| ())
    }

    /// Send a request and await its matching response (used for the handshake,
    /// before the event stream starts). Non-matching lines are ignored.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.take_id();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_value(&request).await?;

        let reader = self
            .reader
            .as_mut()
            .ok_or("client already switched to streaming mode")?;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                return Err("server closed the connection".to_string());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Skip notifications (events) while waiting for our response.
            if value.get("method").is_some() {
                continue;
            }
            if value.get("id") == Some(&json!(id)) {
                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string());
                    return Err(message);
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            // Response for a different id — ignore.
        }
    }

    /// Consume the read half into a background task that forwards every
    /// `method == "event"` payload for `subscription_id` to the returned
    /// receiver. The task ends (and the channel closes) when the socket closes.
    pub fn start_event_pump(&mut self, subscription_id: u64) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let Some(mut reader) = self.reader.take() else {
            drop(tx);
            return rx;
        };
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if value.get("method").and_then(Value::as_str) != Some("event") {
                    continue;
                }
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                if params.get("subscriptionId").and_then(Value::as_u64) != Some(subscription_id) {
                    continue;
                }
                let event = params.get("event").cloned().unwrap_or(Value::Null);
                if tx.send(event).is_err() {
                    break;
                }
                if trimmed.contains("\"session_end\"") {
                    break;
                }
            }
        });
        rx
    }

    /// Forward a raw `--mode rpc` command object to the session.
    pub async fn send_command(&mut self, session_id: &str, command: Value) -> Result<(), String> {
        let id = self.take_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "send",
            "params": {"sessionId": session_id, "command": command},
        });
        self.write_value(&request).await
    }

    /// Best-effort session teardown.
    pub async fn stop_session(&mut self, session_id: &str) {
        let id = self.take_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "stop_session",
            "params": {"sessionId": session_id},
        });
        let _ = self.write_value(&request).await;
    }
}
