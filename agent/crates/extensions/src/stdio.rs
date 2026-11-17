//! Exclusive stdio ownership and completion, independent of the served protocol.
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    owner: Option<(u64, String)>,
    exit_code: Option<i32>,
    conflict: Option<String>,
}

#[derive(Clone, Default)]
pub struct StdioLifecycle {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

impl StdioLifecycle {
    pub(crate) fn claim(&self, id: u64, name: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        match &state.owner {
            Some((owner, _)) if *owner == id => Ok(()),
            Some((_, owner)) => {
                let error =
                    format!("stdio is already owned by `{owner}`; `{name}` cannot claim it");
                state.conflict = Some(error.clone());
                Err(error)
            }
            None => {
                state.owner = Some((id, name.to_owned()));
                Ok(())
            }
        }
    }

    pub(crate) fn release_failed_registration(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        if state.owner.as_ref().is_some_and(|(owner, _)| *owner == id) {
            state.owner = None;
            state.exit_code = None;
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .conflict
            .clone()
            .map_or(Ok(()), Err)
    }

    pub(crate) fn handle(&self, id: u64, args: Value) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap();
        match args.get("op").and_then(Value::as_str) {
            Some("status") => Ok(
                json!({"owner": state.owner.as_ref().map(|(_, name)| name), "exitCode": state.exit_code}),
            ),
            Some("finish") => {
                if !state.owner.as_ref().is_some_and(|(owner, _)| *owner == id) {
                    return Err("only the stdio owner may finish its lifecycle".to_owned());
                }
                let code = args
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok())
                    .ok_or("finish requires an i32 exitCode")?;
                if state.exit_code.is_some_and(|previous| previous != code) {
                    return Err(
                        "stdio lifecycle already finished with a different exitCode".to_owned()
                    );
                }
                state.exit_code = Some(code);
                drop(state);
                self.changed.notify_waiters();
                Ok(json!({"ok": true}))
            }
            _ => Err("stdio lifecycle op must be status or finish".to_owned()),
        }
    }

    /// Completion is retained, including when a plugin finishes before the host waits.
    pub async fn wait(&self) -> i32 {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(code) = self.state.lock().unwrap().exit_code {
                return code;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owner_finishes_before_or_after_host_waits() {
        let lifecycle = StdioLifecycle::default();
        lifecycle.claim(1, "test-server").unwrap();
        assert!(lifecycle
            .handle(2, json!({"op":"finish","exitCode":0}))
            .is_err());
        let waiting = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move { lifecycle.wait().await }
        });
        lifecycle
            .handle(1, json!({"op":"finish","exitCode":7}))
            .unwrap();
        assert_eq!(waiting.await.unwrap(), 7);
        assert_eq!(lifecycle.wait().await, 7);
    }

    #[test]
    fn duplicate_names_still_have_exclusive_owners() {
        let lifecycle = StdioLifecycle::default();
        lifecycle.claim(1, "server").unwrap();
        lifecycle.claim(1, "server").unwrap();
        assert!(lifecycle.claim(2, "server").is_err());
        assert!(lifecycle.validate().is_err());
    }

    #[test]
    fn failed_registration_releases_its_claim() {
        let lifecycle = StdioLifecycle::default();
        lifecycle.claim(1, "failed").unwrap();
        lifecycle.release_failed_registration(1);
        lifecycle.claim(2, "working").unwrap();
        assert!(lifecycle.validate().is_ok());
    }
}
