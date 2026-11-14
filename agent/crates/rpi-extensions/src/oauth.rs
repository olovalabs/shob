//! Host-side bridge for provider-owned OAuth flows.
//!
//! The host owns discovery and callback invocation, while platform-specific
//! authorization remains in the plugin. Token-bearing JSON is returned to the
//! caller only; this module never logs it or emits it as an agent event.

use std::ffi::c_void;
use std::sync::Arc;

use rpi_plugin_sdk::{OAuthRequestControl, StbString, StbStringRef};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::registry::{RegisteredOAuthProvider, RegistrySnapshot};

#[derive(Debug, thiserror::Error)]
pub enum OAuthExtensionError {
    #[error("OAuth provider `{0}` is not registered")]
    NotFound(String),
    #[error("OAuth request was cancelled")]
    Cancelled,
    #[error("OAuth response has unsupported version")]
    UnsupportedVersion,
    #[error("OAuth response has invalid kind")]
    InvalidResponseKind,
    #[error("OAuth action must be one of begin, exchange, refresh, revoke")]
    InvalidAction,
    #[error("OAuth plugin panicked during request")]
    PluginPanic,
    #[error("OAuth plugin returned status {0}: {1}")]
    PluginStatus(i32, String),
    #[error("OAuth plugin returned invalid JSON: {0}")]
    InvalidResponse(String),
}

/// Standard response kinds understood by the host. Payload contents remain
/// provider-defined except for `credential`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthResponseKind {
    Interaction,
    Credential,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct OAuthActionResponse {
    pub kind: OAuthResponseKind,
    pub payload: Value,
}

impl OAuthActionResponse {
    pub fn parse(value: Value) -> Result<Self, OAuthExtensionError> {
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
        if version != 1 {
            return Err(OAuthExtensionError::UnsupportedVersion);
        }
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some("interaction") => OAuthResponseKind::Interaction,
            Some("credential") => OAuthResponseKind::Credential,
            Some("success") => OAuthResponseKind::Success,
            Some("error") => OAuthResponseKind::Error,
            _ => return Err(OAuthExtensionError::InvalidResponseKind),
        };
        Ok(Self {
            kind,
            payload: value.get("payload").cloned().unwrap_or(Value::Null),
        })
    }

    /// Parse the host-owned credential envelope. Provider-specific token
    /// aliases must be converted by the extension before this point.
    pub fn credential(&self) -> Result<Option<OAuthCredential>, OAuthExtensionError> {
        if self.kind != OAuthResponseKind::Credential {
            return Ok(None);
        }
        let object = self.payload.as_object().ok_or_else(|| {
            OAuthExtensionError::InvalidResponse("credential payload must be an object".into())
        })?;
        let access = object
            .get("access")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                OAuthExtensionError::InvalidResponse("missing credential access".into())
            })?;
        let refresh = object
            .get("refresh")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expires_at = object
            .get("expires_at")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Ok(Some(OAuthCredential {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_at,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires_at: i64,
}

/// A non-secret view of a registered OAuth provider.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthProviderInfo {
    pub plugin: String,
    pub id: String,
    #[serde(flatten)]
    pub manifest: serde_json::Map<String, Value>,
}

/// List OAuth manifests without exposing callbacks or user data.
pub fn list(snapshot: &RegistrySnapshot) -> Vec<OAuthProviderInfo> {
    snapshot.oauth_providers().iter().filter_map(info).collect()
}

/// Invoke one provider-owned OAuth action.
///
/// `params` is action-specific and may contain transient authorization data.
/// The returned value is intentionally not logged by this layer.
pub fn request(
    snapshot: &Arc<RegistrySnapshot>,
    provider_id: &str,
    action: &str,
    params: Value,
) -> Result<Value, OAuthExtensionError> {
    request_with_cancel(
        snapshot,
        provider_id,
        action,
        params,
        &CancellationToken::new(),
    )
}

/// Invoke one provider-owned OAuth action that the caller may cancel.
///
/// A provider registered with `register_oauth_provider_cancellable` receives a
/// control that reports `cancel`, and is expected to return promptly once it
/// does. A legacy provider cannot observe cancellation, so the call still runs
/// to its own completion. In both cases a call that was cancelled returns
/// [`OAuthExtensionError::Cancelled`], and any output the plugin produced is
/// freed unread. A call that is already cancelled never reaches the plugin.
pub fn request_with_cancel(
    snapshot: &Arc<RegistrySnapshot>,
    provider_id: &str,
    action: &str,
    params: Value,
    cancel: &CancellationToken,
) -> Result<Value, OAuthExtensionError> {
    if !matches!(action, "begin" | "exchange" | "refresh" | "revoke") {
        return Err(OAuthExtensionError::InvalidAction);
    }
    let provider = snapshot
        .oauth_provider(provider_id)
        .ok_or_else(|| OAuthExtensionError::NotFound(provider_id.to_string()))?;
    invoke(provider, action, params, cancel)
}

fn info(provider: &RegisteredOAuthProvider) -> Option<OAuthProviderInfo> {
    let mut manifest = serde_json::from_str::<Value>(&provider.manifest_json)
        .ok()?
        .as_object()?
        .clone();
    let id = manifest.get("id").and_then(Value::as_str)?.to_string();
    manifest.remove("id");
    Some(OAuthProviderInfo {
        plugin: provider.plugin.clone(),
        id,
        manifest,
    })
}

fn invoke(
    provider: &RegisteredOAuthProvider,
    action: &str,
    params: Value,
    cancel: &CancellationToken,
) -> Result<Value, OAuthExtensionError> {
    if cancel.is_cancelled() {
        return Err(OAuthExtensionError::Cancelled);
    }
    let request = serde_json::json!({
        "action": action,
        "providerId": provider_id(provider),
        "input": params,
    })
    .to_string();
    let mut out = StbString::empty();
    let status =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match provider.control_fn {
            Some(request_fn) => {
                // The token is borrowed by this frame until the plugin call returns,
                // so the control pointer stays valid for the whole call.
                let control = OAuthRequestControl {
                    struct_size: std::mem::size_of::<OAuthRequestControl>() as u32,
                    is_cancelled: control_is_cancelled,
                    user_data: cancel as *const CancellationToken as *mut c_void,
                };
                request_fn(
                    StbStringRef::from_str(&request),
                    &control,
                    &mut out,
                    provider.user_data,
                )
            }
            None => (provider.request_fn)(
                StbStringRef::from_str(&request),
                &mut out,
                provider.user_data,
            ),
        }));
    // SAFETY: the callback transfers a live output to its registered producer.
    // `out` remains accessible if the callback panics, so the plugin allocation
    // is reclaimed on both the success and panic paths.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(provider.plugin_free_string))
    };
    if cancel.is_cancelled() {
        // The caller stopped waiting, so whatever the plugin produced is dropped
        // unread, including a credential from an exchange that finished late.
        return Err(OAuthExtensionError::Cancelled);
    }
    let response = out.to_string_lossy();
    let status = match status {
        Ok(status) => status,
        Err(_) => return Err(OAuthExtensionError::PluginPanic),
    };
    if status != 0 {
        return Err(OAuthExtensionError::PluginStatus(status, response));
    }
    serde_json::from_str(&response)
        .map_err(|error| OAuthExtensionError::InvalidResponse(error.to_string()))
}

/// Cancellation poll handed to a control-aware plugin. `user_data` is the
/// `CancellationToken` borrowed by `invoke` for the duration of the plugin call.
extern "C" fn control_is_cancelled(user_data: *mut c_void) -> i32 {
    std::panic::catch_unwind(|| {
        if user_data.is_null() {
            return 0;
        }
        // SAFETY: `invoke` keeps the token borrowed until the plugin call returns.
        let token = unsafe { &*user_data.cast::<CancellationToken>() };
        i32::from(token.is_cancelled())
    })
    .unwrap_or(0)
}

fn provider_id(provider: &RegisteredOAuthProvider) -> String {
    // Registration validates this field, so a malformed value can only be
    // present if a future host mutates the record. Return an empty id rather
    // than panic across an extension boundary.
    serde_json::from_str::<Value>(&provider.manifest_json)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ExtensionRegistry, RegisteredOAuthProvider};
    use rpi_plugin_sdk::{OAuthRequestControl, OAuthRequestControlFn};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static FREED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn free_output(value: StbString) {
        if !value.is_empty() {
            FREED.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: the test callback transfers one live SDK allocation.
        unsafe { rpi_plugin_sdk::callbacks::free_string(value) };
    }

    rpi_plugin_sdk::string_handler! { fn callback(request, out, _user_data) {
        let value: Value = serde_json::from_str(request).unwrap();
        let response = if value.get("action").and_then(Value::as_str) == Some("refresh") {
            r#"{"access":"secret"}"#
        } else {
            r#"{"ok":true}"#
        };
        i32::from(!out.write(response.to_string()))
    } }

    fn snapshot() -> Arc<RegistrySnapshot> {
        let mut registry = ExtensionRegistry::new();
        registry.register_oauth_provider(RegisteredOAuthProvider {
            plugin: "oauth-test".to_string(),
            manifest_json: r#"{"id":"acme","displayName":"Acme"}"#.to_string(),
            request_fn: callback,
            control_fn: None,
            plugin_free_string: free_output,
            user_data: std::ptr::null_mut(),
        });
        Arc::new(registry.snapshot())
    }

    #[test]
    fn lists_non_secret_manifest_info() {
        let providers = list(&snapshot());
        assert_eq!(providers[0].id, "acme");
        assert_eq!(providers[0].plugin, "oauth-test");
        assert!(!providers[0].manifest.contains_key("id"));
    }

    #[test]
    fn invokes_action_and_reclaims_plugin_output() {
        FREED.store(0, Ordering::SeqCst);
        let result = request(&snapshot(), "acme", "refresh", serde_json::json!({"x": 1})).unwrap();
        assert_eq!(result["access"], "secret");
        assert_eq!(FREED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parses_standard_interaction_and_credential_responses() {
        let interaction = OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "interaction",
            "payload": {"display": {"url": "opaque"}}
        }))
        .unwrap();
        assert_eq!(interaction.kind, OAuthResponseKind::Interaction);

        let credential = OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "credential",
            "payload": {"access": "a", "refresh": "r", "expires_at": 42}
        }))
        .unwrap();
        assert_eq!(credential.credential().unwrap().unwrap().expires_at, 42);
    }

    #[test]
    fn rejects_unknown_response_version_and_kind() {
        assert!(matches!(
            OAuthActionResponse::parse(serde_json::json!({"version": 2, "kind": "success"})),
            Err(OAuthExtensionError::UnsupportedVersion)
        ));
        assert!(matches!(
            OAuthActionResponse::parse(
                serde_json::json!({"version": 1, "kind": "provider-specific"})
            ),
            Err(OAuthExtensionError::InvalidResponseKind)
        ));
    }

    #[test]
    fn rejects_unknown_action_and_provider() {
        assert!(matches!(
            request(&snapshot(), "acme", "unknown", Value::Null),
            Err(OAuthExtensionError::InvalidAction)
        ));
        assert!(matches!(
            request(&snapshot(), "missing", "begin", Value::Null),
            Err(OAuthExtensionError::NotFound(_))
        ));
    }

    /// Output counter for the cancellation tests. Kept apart from `FREED`, which
    /// the reclamation test resets while the other tests run in parallel.
    static CANCEL_FREED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn free_cancel_output(value: StbString) {
        if !value.is_empty() {
            CANCEL_FREED.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: the test callback transfers one live SDK allocation.
        unsafe { rpi_plugin_sdk::callbacks::free_string(value) };
    }

    /// Blocks in `exchange` until the host cancels its control: the plugin
    /// behaviour a host-side timeout cannot reach.
    extern "C" fn blocking_exchange(
        _request: StbStringRef,
        control: *const OAuthRequestControl,
        _out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        let started = Instant::now();
        // SAFETY: the host lends a live control for the duration of this call.
        while !unsafe { OAuthRequestControl::poll_cancelled(control) } {
            // A safety valve so a regression fails instead of hanging the suite.
            if started.elapsed() > Duration::from_secs(10) {
                return 0;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        SAW_CANCEL.store(true, Ordering::SeqCst);
        1
    }

    static SAW_CANCEL: AtomicBool = AtomicBool::new(false);

    extern "C" fn unused_legacy(
        _request: StbStringRef,
        _out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        1
    }

    fn controlled_snapshot(request_fn: OAuthRequestControlFn) -> Arc<RegistrySnapshot> {
        let mut registry = ExtensionRegistry::new();
        registry.register_oauth_provider(RegisteredOAuthProvider {
            plugin: "cancel-test".to_string(),
            manifest_json: r#"{"id":"acme"}"#.to_string(),
            request_fn: unused_legacy,
            control_fn: Some(request_fn),
            plugin_free_string: free_cancel_output,
            user_data: std::ptr::null_mut(),
        });
        Arc::new(registry.snapshot())
    }

    #[test]
    fn cancelled_exchange_returns_within_one_second() {
        SAW_CANCEL.store(false, Ordering::SeqCst);
        let snapshot = controlled_snapshot(blocking_exchange);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.cancel();
        });
        let started = Instant::now();
        let result = request_with_cancel(
            &snapshot,
            "acme",
            "exchange",
            serde_json::json!({"value": "code"}),
            &cancel,
        );
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert!(
            SAW_CANCEL.load(Ordering::SeqCst),
            "the plugin never observed the host cancellation"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "cancelled exchange took {elapsed:?}"
        );
    }

    static PRECANCEL_CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn counting_control(
        _request: StbStringRef,
        _control: *const OAuthRequestControl,
        _out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        PRECANCEL_CALLS.fetch_add(1, Ordering::SeqCst);
        0
    }

    #[test]
    fn precancelled_action_never_reaches_the_plugin() {
        PRECANCEL_CALLS.store(0, Ordering::SeqCst);
        let snapshot = controlled_snapshot(counting_control);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result =
            request_with_cancel(&snapshot, "acme", "begin", serde_json::json!({}), &cancel);
        assert!(matches!(result, Err(OAuthExtensionError::Cancelled)));
        assert_eq!(PRECANCEL_CALLS.load(Ordering::SeqCst), 0);
    }

    /// A legacy callback has no control, so the host cannot stop it mid-call.
    /// The cancellation it races with still turns its output into `Cancelled`.
    static LEGACY_CANCEL: Mutex<Option<CancellationToken>> = Mutex::new(None);

    extern "C" fn legacy_cancelling(
        _request: StbStringRef,
        out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        if let Some(token) = LEGACY_CANCEL.lock().unwrap().take() {
            token.cancel();
        }
        // SAFETY: `out` is the host's exclusive output slot for this call.
        unsafe {
            *out = StbString::from_string(
                r#"{"version":1,"kind":"credential","payload":{"access":"a"}}"#.to_string(),
            )
        };
        0
    }

    #[test]
    fn legacy_call_cancelled_in_flight_returns_cancelled_and_frees_its_output() {
        CANCEL_FREED.store(0, Ordering::SeqCst);
        let mut registry = ExtensionRegistry::new();
        registry.register_oauth_provider(RegisteredOAuthProvider {
            plugin: "cancel-test".to_string(),
            manifest_json: r#"{"id":"legacy"}"#.to_string(),
            request_fn: legacy_cancelling,
            control_fn: None,
            plugin_free_string: free_cancel_output,
            user_data: std::ptr::null_mut(),
        });
        let snapshot = Arc::new(registry.snapshot());
        let cancel = CancellationToken::new();
        *LEGACY_CANCEL.lock().unwrap() = Some(cancel.clone());
        let result = request_with_cancel(
            &snapshot,
            "legacy",
            "exchange",
            serde_json::json!({}),
            &cancel,
        );
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(CANCEL_FREED.load(Ordering::SeqCst), 1);
    }
}
