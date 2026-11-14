//! Transport-neutral pending client operations. The waiter owns retirement;
//! dropping a future, timeout, cancellation and shutdown cannot leak a slot.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const MAX_PENDING: usize = 64;
pub(crate) const MAX_TIMEOUT_MS: u64 = 60_000;

type Reply = Result<Value, String>;
type Pending = HashMap<String, Option<oneshot::Sender<Reply>>>;

#[derive(Clone, Default)]
pub(crate) struct ClientRequestBroker {
    pending: Arc<Mutex<Pending>>,
}

pub(crate) struct ClientRequest {
    broker: ClientRequestBroker,
    pub(crate) id: String,
    receiver: oneshot::Receiver<Reply>,
}

impl ClientRequestBroker {
    pub(crate) fn register(&self) -> Result<ClientRequest, String> {
        let mut pending = self.pending.lock().expect("client request broker poisoned");
        if pending.len() >= MAX_PENDING {
            return Err("too many pending client requests".into());
        }
        // Never reuse caller IDs or method names: a late response must not
        // resolve a subsequent operation, even for the same method.
        let id = format!("native:{}", uuid::Uuid::new_v4());
        let (sender, receiver) = oneshot::channel();
        pending.insert(id.clone(), Some(sender));
        Ok(ClientRequest {
            broker: self.clone(),
            id,
            receiver,
        })
    }

    pub(crate) fn resolve(&self, id: &str, reply: Reply) -> bool {
        self.pending
            .lock()
            .expect("client request broker poisoned")
            .get_mut(id)
            .and_then(Option::take)
            .is_some_and(|sender| sender.send(reply).is_ok())
    }

    pub(crate) fn cancel_all(&self) {
        self.pending
            .lock()
            .expect("client request broker poisoned")
            .clear();
    }
}

impl ClientRequest {
    pub(crate) async fn wait(mut self, timeout: Duration, cancel: &CancellationToken) -> Reply {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("client request cancelled".into()),
            result = &mut self.receiver => result.unwrap_or_else(|_| Err("client request broker closed".into())),
            _ = tokio::time::sleep(timeout) => Err("client request timed out".into()),
        }
    }
}

impl Drop for ClientRequest {
    fn drop(&mut self) {
        self.broker
            .pending
            .lock()
            .expect("client request broker poisoned")
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_admission_drop_and_duplicate_replies() {
        let broker = ClientRequestBroker::default();
        let requests: Vec<_> = (0..MAX_PENDING)
            .map(|_| broker.register().unwrap())
            .collect();
        assert!(broker.register().is_err());
        let id = requests[0].id.clone();
        assert!(broker.resolve(&id, Ok(Value::Null)));
        assert!(!broker.resolve(&id, Ok(Value::Null)));
        assert!(
            broker.register().is_err(),
            "resolved but unconsumed requests still own slots"
        );
        drop(requests);
        assert!(broker.pending.lock().unwrap().is_empty());
        assert!(!broker.resolve(&id, Ok(Value::Null)));
        assert!(broker.register().is_ok());
    }

    #[tokio::test]
    async fn timeout_cancellation_shutdown_and_aborted_wait_retire() {
        let broker = ClientRequestBroker::default();
        let cancel = CancellationToken::new();
        let request = broker.register().unwrap();
        let id = request.id.clone();
        assert!(request
            .wait(Duration::from_millis(1), &cancel)
            .await
            .unwrap_err()
            .contains("timed out"));
        assert!(!broker.resolve(&id, Ok(Value::Null)));
        assert!(broker.pending.lock().unwrap().is_empty());

        let request = broker.register().unwrap();
        cancel.cancel();
        assert!(request
            .wait(Duration::from_secs(1), &cancel)
            .await
            .unwrap_err()
            .contains("cancelled"));
        assert!(broker.pending.lock().unwrap().is_empty());

        let request = broker.register().unwrap();
        broker.cancel_all();
        assert!(request
            .wait(Duration::from_secs(1), &CancellationToken::new())
            .await
            .unwrap_err()
            .contains("closed"));
        let request = broker.register().unwrap();
        let id = request.id.clone();
        let task = tokio::spawn(async move {
            request
                .wait(Duration::from_secs(60), &CancellationToken::new())
                .await
        });
        task.abort();
        let _ = task.await;
        assert!(broker.pending.lock().unwrap().is_empty());
        assert!(!broker.resolve(&id, Ok(Value::Null)));
    }
}
