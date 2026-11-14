//! Single-owner bounded stdout writer for the native RPC transport.
use std::io::Write;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct RpcWriter {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    failed: CancellationToken,
    mode: crate::args::RpcMessageUpdates,
}

pub(crate) fn start_rpc_writer<W: Write + Send + 'static>(
    mut output: W,
    mode: crate::args::RpcMessageUpdates,
) -> (
    RpcWriter,
    tokio::task::JoinHandle<std::io::Result<()>>,
    CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let failed = CancellationToken::new();
    let failure_signal = failed.clone();
    let runtime = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        let result = (|| {
            loop {
                let line = runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = failure_signal.cancelled() => None,
                        line = rx.recv() => line,
                    }
                });
                let Some(line) = line else { break };
                // An OS write already in progress cannot be hard-preempted.
                output.write_all(&line)?;
                output.flush()?;
            }
            if failure_signal.is_cancelled() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "RPC transport failed",
                ))
            } else {
                output.flush()
            }
        })();
        if result.is_err() {
            failure_signal.cancel();
        }
        result
    });
    (
        RpcWriter {
            sender: tx,
            failed: failed.clone(),
            mode,
        },
        task,
        failed,
    )
}

fn encode_line(value: &serde_json::Value) -> Vec<u8> {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    line
}

impl RpcWriter {
    pub(crate) fn failure(&self) -> CancellationToken {
        self.failed.clone()
    }

    async fn enqueue(&self, line: Vec<u8>) -> std::io::Result<()> {
        tokio::select! {
            biased;
            _ = self.failed.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "RPC writer failed")),
            result = self.sender.send(line) => result.map_err(|_| {
                self.failed.cancel();
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "RPC writer closed")
            }),
        }
    }

    /// Only external requests have an enqueue deadline. Ordinary healthy
    /// producers retain backpressure, but all wake on authoritative failure.
    pub(crate) async fn write_bounded(&self, value: &serde_json::Value) -> std::io::Result<()> {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            self.enqueue(encode_line(value)),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.failed.cancel();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "RPC external request queue stalled",
                ))
            }
        }
    }

    /// Reserve external-request capacity without publishing any bytes. Dropping
    /// this future (or its permit) has no output side effects. The caller can
    /// commit the permit synchronously under its own lifecycle gate.
    pub(crate) async fn reserve_bounded(
        &self,
    ) -> std::io::Result<tokio::sync::mpsc::OwnedPermit<Vec<u8>>> {
        let reserve = async {
            tokio::select! {
                biased;
                _ = self.failed.cancelled() => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe, "RPC writer failed",
                )),
                result = self.sender.clone().reserve_owned() => result.map_err(|_| {
                    self.failed.cancel();
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "RPC writer closed")
                }),
            }
        };
        match tokio::time::timeout(std::time::Duration::from_millis(200), reserve).await {
            Ok(result) => result,
            Err(_) => {
                self.failed.cancel();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "RPC external request queue stalled",
                ))
            }
        }
    }

    /// RAII closure notification cannot await. Failure to queue is an
    /// authoritative transport failure, not a silently lost notification.
    pub(crate) fn try_write_line(&self, value: &serde_json::Value) {
        if !self.failed.is_cancelled() && self.sender.try_send(encode_line(value)).is_err() {
            self.failed.cancel();
        }
    }
}

pub(crate) async fn write_line(out: &RpcWriter, value: &serde_json::Value) {
    let _ = out.enqueue(encode_line(value)).await;
}

pub(crate) async fn write_agent_event(out: &RpcWriter, event: &rpi_agent::events::AgentEvent) {
    let mut line = super::events::encode(event, out.mode).unwrap_or_else(|_| b"null".to_vec());
    line.push(b'\n');
    let _ = out.enqueue(line).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("external_tools_writer_tests.rs");
}
