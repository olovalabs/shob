use super::*;
use crate::config::test_support::env_lock;
struct TestEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_key: Option<std::ffi::OsString>,
    prev_tok: Option<std::ffi::OsString>,
    prev_base: Option<std::ffi::OsString>,
    prev_openai_key: Option<std::ffi::OsString>,
    prev_dir: Option<std::ffi::OsString>,
    _tmp: tempfile::TempDir,
}
impl TestEnv {
    fn new() -> Self {
        let guard = env_lock().lock().unwrap();
        let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
        let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
        let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
        let prev_openai_key = std::env::var_os(OPENAI_API_KEY_ENV);
        let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
        std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
        std::env::remove_var(OPENAI_API_KEY_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
        Self {
            _guard: guard,
            prev_key,
            prev_tok,
            prev_base,
            prev_openai_key,
            prev_dir,
            _tmp: tmp,
        }
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        restore(ANTHROPIC_API_KEY_ENV, self.prev_key.take());
        restore(ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
        restore(ANTHROPIC_BASE_URL_ENV, self.prev_base.take());
        restore(OPENAI_API_KEY_ENV, self.prev_openai_key.take());
        restore(config::CONFIG_DIR_ENV, self.prev_dir.take());
    }
}
fn restore(name: &str, prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}

#[test]
fn trusted_project_defaults_override_global_defaults_for_resolution() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "global": {
      "api": "anthropic-messages",
      "apiKey": "global-secret",
      "models": [{"id":"global-model"}]
    },
    "project": {
      "api": "anthropic-messages",
      "apiKey": "project-secret",
      "models": [{"id":"project-model"}]
    }
  }
}"#,
    )
    .unwrap();
    std::fs::write(
        config::settings_path().unwrap(),
        r#"{"defaultProvider":"global","defaultModel":"global-model"}"#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".rpi")).unwrap();
    std::fs::write(
        project.path().join(".rpi/settings.json"),
        r#"{"defaultProvider":"project","defaultModel":"project-model"}"#,
    )
    .unwrap();

    let trusted = resolve_for_cwd(None, None, None, None, None, project.path(), true).unwrap();
    assert_eq!(trusted.provider.id(), "project");
    assert_eq!(trusted.model.id, "project-model");

    let untrusted = resolve_for_cwd(None, None, None, None, None, project.path(), false).unwrap();
    assert_eq!(untrusted.provider.id(), "global");
    assert_eq!(untrusted.model.id, "global-model");
}

/// A provider package that owns an OAuth flow. Stored OAuth credentials expire;
/// the package's `refresh` action, not the user, should renew them.
mod package_oauth_refresh {
    use super::*;
    use rpi_extensions::{
        ExtensionRegistry, ExtensionSession, PluginKeepalive, RegisteredOAuthProvider,
        RegisteredProvider,
    };
    use rpi_plugin_sdk::{callbacks::free_string, StbString, StbStringRef};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};

    static FAIL_REFRESH: AtomicBool = AtomicBool::new(false);
    static SLOW_ENTERED: AtomicBool = AtomicBool::new(false);
    static SLOW_RELEASE: AtomicBool = AtomicBool::new(false);
    static CORRUPT_ON_REFRESH: AtomicBool = AtomicBool::new(false);
    static REFRESH_INPUTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    static REFRESH_RESPONSE: Mutex<Option<Value>> = Mutex::new(None);
    static SENT_KEYS: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());
    const FRESH_EXPIRY: i64 = 4_102_444_800; // 2100-01-01

    fn reply(out: *mut StbString, body: Value) {
        // SAFETY: the host passes an exclusive output slot for this call.
        unsafe { *out = StbString::from_string(body.to_string()) };
    }

    extern "C" fn oauth(
        request: StbStringRef,
        out: *mut StbString,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the host keeps the request buffer valid for this call.
        let request: Value = serde_json::from_str(unsafe { request.as_str() }).unwrap_or_default();
        // "blocked-refresh" models a hung OAuth endpoint until the test releases it.
        if request["input"]["credential"]["refresh"] == "blocked-refresh" {
            SLOW_ENTERED.store(true, SeqCst);
            while !SLOW_RELEASE.load(SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        REFRESH_INPUTS.lock().unwrap().push(request);
        if FAIL_REFRESH.load(SeqCst) {
            reply(
                out,
                json!({"version": 1, "kind": "error",
                "payload": {"message": "stale-access refresh-1 malicious-secret"}}),
            );
            return 1;
        }
        if CORRUPT_ON_REFRESH.load(SeqCst) {
            std::fs::write(config::auth_path().unwrap(), "{ corrupt auth").unwrap();
        }
        let response = REFRESH_RESPONSE.lock().unwrap().clone().unwrap_or_else(|| {
            json!({"version": 1, "kind": "credential", "payload": {
                "access": "fresh-access", "refresh": "refresh-2", "expires_at": FRESH_EXPIRY}})
        });
        reply(out, response);
        0
    }

    extern "C" fn provider(
        request: StbStringRef,
        out: *mut StbString,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the host keeps the request buffer valid for this call.
        let request: Value = serde_json::from_str(unsafe { request.as_str() }).unwrap_or_default();
        let key = request["options"]["apiKey"].as_str().map(str::to_owned);
        SENT_KEYS.lock().unwrap().push(key);
        reply(
            out,
            json!({
                "role": "assistant", "content": [{"type": "text", "text": "ok"}],
                "api": "google-generative-ai", "provider": "acme", "model": "acme-model",
                "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                    "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
                "stopReason": "stop", "timestamp": 0,
            }),
        );
        0
    }

    fn session(with_oauth: bool) -> ExtensionSession {
        FAIL_REFRESH.store(false, SeqCst);
        CORRUPT_ON_REFRESH.store(false, SeqCst);
        REFRESH_INPUTS.lock().unwrap().clear();
        *REFRESH_RESPONSE.lock().unwrap() = None;
        SENT_KEYS.lock().unwrap().clear();
        let mut registry = ExtensionRegistry::new();
        registry.register_provider(RegisteredProvider {
            provider_id: "acme".into(),
            base_url: "http://127.0.0.1:9".into(),
            api_style: "google-generative-ai".into(),
            request_fn: provider,
            stream_fn: None,
            plugin_free_string: free_string,
            user_data: std::ptr::null_mut(),
        });
        let model = Model::new(
            "acme-model",
            "Acme",
            rpi_ai::Api::GoogleGenerativeAi,
            "acme",
            "http://127.0.0.1:9",
        );
        assert!(registry.apply_declaration_json(
            &json!({"providers": [{"id": "acme", "requiresAuth": true, "models": [model]}]})
                .to_string()
        ));
        if with_oauth {
            assert!(!registry.register_oauth_provider(RegisteredOAuthProvider {
                plugin: "acme".into(),
                manifest_json: r#"{"id":"acme","displayName":"Acme"}"#.into(),
                request_fn: oauth,
                control_fn: None,
                plugin_free_string: free_string,
                user_data: std::ptr::null_mut(),
            }));
        }
        ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        )
    }

    fn resolve(session: &ExtensionSession) -> Result<ResolvedModel, ResolveError> {
        let args = crate::args::Args {
            provider: Some("acme".into()),
            model: Some("acme-model".into()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().unwrap();
        resolve_with_extensions(&args, cwd.path(), false, session)
    }

    async fn ask(resolved: &ResolvedModel) -> rpi_ai::AssistantMessage {
        resolved
            .provider
            .stream_simple(
                &resolved.model,
                &rpi_ai::Context::default(),
                &rpi_ai::SimpleStreamOptions::default(),
            )
            .await
            .result()
            .await
            .unwrap()
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn stored() -> (String, String, i64) {
        match config::read_oauth_token("acme").unwrap() {
            Some(config::Credential::Oauth {
                access,
                refresh,
                expires,
            }) => (access, refresh, expires),
            other => panic!(
                "expected a stored OAuth credential, got {}",
                other.is_some()
            ),
        }
    }

    fn sent() -> Vec<Option<String>> {
        SENT_KEYS.lock().unwrap().clone()
    }

    fn refreshes() -> usize {
        REFRESH_INPUTS.lock().unwrap().len()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_credential_is_refreshed_once_before_the_request() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();

        let resolved = resolve(&session).expect("a refreshable credential selects the provider");
        assert_eq!(refreshes(), 0, "resolution itself never calls the package");
        assert_eq!(ask(&resolved).await.stop_reason, rpi_ai::StopReason::Stop);
        assert_eq!(sent(), [Some("fresh-access".to_owned())]);
        let input = REFRESH_INPUTS.lock().unwrap()[0].clone();
        assert_eq!(input["action"], "refresh");
        assert_eq!(input["input"]["credential"]["refresh"], "refresh-1");
        assert_eq!(
            stored(),
            ("fresh-access".into(), "refresh-2".into(), FRESH_EXPIRY)
        );

        ask(&resolved).await;
        assert_eq!(refreshes(), 1, "a fresh credential is reused");
        assert_eq!(sent()[1].as_deref(), Some("fresh-access"));
    }

    /// Two OAuth provider identities, `acme` and `beta`, served by the same fixture.
    fn two_oauth_providers() -> Arc<crate::auth::PackageOAuthRefresher> {
        let mut registry = ExtensionRegistry::new();
        for id in ["acme", "beta"] {
            registry.register_oauth_provider(RegisteredOAuthProvider {
                plugin: id.into(),
                manifest_json: json!({"id": id, "displayName": id}).to_string(),
                request_fn: oauth,
                control_fn: None,
                plugin_free_string: free_string,
                user_data: std::ptr::null_mut(),
            });
        }
        let session = ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        );
        Arc::new(crate::auth::PackageOAuthRefresher::new(&session).unwrap())
    }

    #[test]
    fn a_slow_refresh_does_not_block_another_provider() {
        use rpi_runtime::credentials::OAuthRefresher;
        let _env = TestEnv::new();
        let _reset = session(true);
        SLOW_ENTERED.store(false, SeqCst);
        SLOW_RELEASE.store(false, SeqCst);
        let refresher = two_oauth_providers();
        config::upsert_oauth_token("acme", "old-a".into(), "blocked-refresh".into(), 1).unwrap();
        config::upsert_oauth_token("beta", "old-b".into(), "healthy-refresh".into(), 1).unwrap();

        let a = refresher.clone();
        let slow = std::thread::spawn(move || a.refresh("acme"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !SLOW_ENTERED.load(SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let entered = SLOW_ENTERED.load(SeqCst);

        let (tx, rx) = std::sync::mpsc::channel();
        let b = refresher.clone();
        let healthy = std::thread::spawn(move || {
            tx.send(b.refresh("beta").is_ok()).unwrap();
        });
        let independent = rx.recv_timeout(std::time::Duration::from_secs(1));

        // Release the hung callback before asserting so no worker is left behind.
        SLOW_RELEASE.store(true, SeqCst);
        assert!(slow.join().unwrap().is_ok());
        healthy.join().unwrap();
        assert!(entered, "slow callback was not entered");
        assert_eq!(
            independent.ok(),
            Some(true),
            "a slow acme OAuth endpoint must not prevent beta renewal"
        );
    }

    #[test]
    fn two_providers_renewing_at_once_both_keep_their_credentials() {
        use rpi_runtime::credentials::OAuthRefresher;
        let _env = TestEnv::new();
        let _reset = session(true);
        let refresher = two_oauth_providers();
        // Each commit is a read-modify-write of the whole store, so a race shows
        // up as one provider's stale entry surviving the other's commit.
        for round in 0..50 {
            config::upsert_oauth_token("acme", "old-a".into(), "refresh-a".into(), 1).unwrap();
            config::upsert_oauth_token("beta", "old-b".into(), "refresh-b".into(), 1).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let workers: Vec<_> = ["acme", "beta"]
                .into_iter()
                .map(|id| {
                    let (refresher, barrier) = (refresher.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        refresher.refresh(id)
                    })
                })
                .collect();
            for worker in workers {
                assert!(worker.join().unwrap().is_ok(), "round {round}");
            }
            for id in ["acme", "beta"] {
                match config::read_oauth_token(id).unwrap() {
                    Some(config::Credential::Oauth {
                        access, expires, ..
                    }) => {
                        assert_eq!(access, "fresh-access", "round {round}, {id}");
                        assert_eq!(expires, FRESH_EXPIRY, "round {round}, {id}");
                    }
                    other => panic!(
                        "round {round}, {id}: lost its credential: {}",
                        other.is_some()
                    ),
                }
            }
        }
        assert_eq!(refreshes(), 100);
    }

    /// Runs a login (`Some((access, refresh, expires))`) or a logout (`None`)
    /// while a refresh of the same provider is paused inside the package, then
    /// reports whether the user's action survived. The worker is always released
    /// and joined first.
    fn auth_change_survives_refresh(login: Option<(&str, &str, i64)>) -> bool {
        use rpi_runtime::credentials::OAuthRefresher;
        let _env = TestEnv::new();
        let _reset = session(true);
        let refresher = two_oauth_providers();
        SLOW_ENTERED.store(false, SeqCst);
        SLOW_RELEASE.store(false, SeqCst);
        config::upsert_oauth_token("acme", "old-access".into(), "blocked-refresh".into(), 1)
            .unwrap();
        let worker = std::thread::spawn(move || refresher.refresh("acme"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !SLOW_ENTERED.load(SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let entered = SLOW_ENTERED.load(SeqCst);
        match login {
            Some((access, refresh, expires)) => {
                config::upsert_oauth_token("acme", access.into(), refresh.into(), expires).unwrap()
            }
            None => assert!(config::delete_credential("acme").unwrap()),
        }
        SLOW_RELEASE.store(true, SeqCst);
        let _ = worker.join().unwrap();
        assert!(entered, "refresh callback did not start");
        let stored = config::read_oauth_token("acme").unwrap();
        match login {
            Some(expected) => matches!(
                stored,
                Some(config::Credential::Oauth {
                    access,
                    refresh,
                    expires,
                }) if (access.as_str(), refresh.as_str(), expires) == expected
            ),
            None => stored.is_none(),
        }
    }

    #[test]
    fn a_refresh_does_not_restore_a_credential_logged_out_meanwhile() {
        assert!(
            auth_change_survives_refresh(None),
            "refresh silently restored the credential after logout"
        );
    }

    #[test]
    fn a_refresh_does_not_overwrite_a_login_saved_meanwhile() {
        assert!(
            auth_change_survives_refresh(Some((
                "new-login-access",
                "new-login-refresh",
                FRESH_EXPIRY
            ))),
            "refresh overwrote a newer login with the old account's result"
        );
    }

    /// Review #144 reproduction: same tokens as the refresh started from, new expiry.
    #[test]
    fn a_refresh_does_not_overwrite_a_login_with_the_same_tokens_and_a_new_expiry() {
        assert!(
            auth_change_survives_refresh(Some(("old-access", "blocked-refresh", FRESH_EXPIRY))),
            "refresh overwrote a login with the same tokens and a new expiry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credential_about_to_expire_is_refreshed_mid_session() {
        let _env = TestEnv::new();
        let session = session(true);
        let soon = now() + 30;
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), soon)
            .unwrap();

        let resolved = resolve(&session).unwrap();
        ask(&resolved).await;
        assert_eq!(sent(), [Some("fresh-access".to_owned())]);
        assert_eq!(refreshes(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_requests_share_one_refresh() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();

        let resolved = resolve(&session).unwrap();
        tokio::join!(ask(&resolved), ask(&resolved), ask(&resolved));
        assert_eq!(refreshes(), 1);
        assert_eq!(sent(), vec![Some("fresh-access".to_owned()); 3]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn independently_resolved_sessions_share_one_refresh() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let first = resolve(&session).unwrap();
        let second = resolve(&session).unwrap();

        tokio::join!(ask(&first), ask(&second));
        assert_eq!(refreshes(), 1);
        assert_eq!(sent(), vec![Some("fresh-access".to_owned()); 2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_keys_bypass_refresh_even_when_equal_to_stored_access() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for key in ["explicit-key", "stale-access"] {
            let args = crate::args::Args {
                provider: Some("acme".into()),
                model: Some("acme-model".into()),
                api_key: Some(key.into()),
                ..Default::default()
            };
            let resolved = resolve_with_extensions(&args, cwd.path(), false, &session).unwrap();
            ask(&resolved).await;
        }
        let resolved = resolve(&session).unwrap();
        let options = rpi_ai::SimpleStreamOptions {
            api_key: Some("request-key".into()),
            ..Default::default()
        };
        resolved
            .provider
            .stream_simple(&resolved.model, &rpi_ai::Context::default(), &options)
            .await
            .result()
            .await
            .unwrap();
        assert_eq!(refreshes(), 0);
        assert_eq!(
            sent(),
            [
                Some("explicit-key".into()),
                Some("stale-access".into()),
                Some("request-key".into())
            ]
        );
        assert_eq!(stored(), ("stale-access".into(), "refresh-1".into(), 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn omitted_refresh_token_preserves_the_previous_token() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        *REFRESH_RESPONSE.lock().unwrap() =
            Some(json!({"version": 1, "kind": "credential", "payload": {
                "access": "fresh-access", "expires_at": FRESH_EXPIRY
            }}));
        ask(&resolve(&session).unwrap()).await;
        assert_eq!(
            stored(),
            ("fresh-access".into(), "refresh-1".into(), FRESH_EXPIRY)
        );
        assert_eq!(sent(), [Some("fresh-access".into())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_refresh_responses_leave_the_store_untouched() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let resolved = resolve(&session).unwrap();
        for payload in [
            json!({"access": "  ", "expires_at": FRESH_EXPIRY}),
            json!({"access": "expired-access", "expires_at": 1}),
            json!({"access": 42, "expires_at": FRESH_EXPIRY}),
            json!({"access": "fresh-access", "expires_at": "invalid"}),
            json!({"access": "fresh-access", "refresh": 42, "expires_at": FRESH_EXPIRY}),
        ] {
            *REFRESH_RESPONSE.lock().unwrap() =
                Some(json!({"version": 1, "kind": "credential", "payload": payload}));
            let message = ask(&resolved).await;
            assert_eq!(message.stop_reason, rpi_ai::StopReason::Error);
            assert_eq!(stored(), ("stale-access".into(), "refresh-1".into(), 1));
            assert!(sent().is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn manual_refresh_renews_even_a_fresh_credential() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token(
            "acme",
            "current-access".into(),
            "refresh-1".into(),
            FRESH_EXPIRY,
        )
        .unwrap();
        let args = ["refresh".into(), "--provider".into(), "acme".into()];
        assert_eq!(
            crate::auth::run_with_extensions(&args, session.snapshot_arc()).await,
            0
        );
        assert_eq!(refreshes(), 1);
        assert_eq!(
            stored(),
            ("fresh-access".into(), "refresh-2".into(), FRESH_EXPIRY)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_failure_does_not_send_credentials_or_expose_them() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let resolved = resolve(&session).unwrap();
        CORRUPT_ON_REFRESH.store(true, SeqCst);
        let message = ask(&resolved).await;
        assert_eq!(message.stop_reason, rpi_ai::StopReason::Error);
        let error = message.error_message.unwrap_or_default();
        assert!(
            error.contains("could not save refreshed OAuth credentials"),
            "{error}"
        );
        for secret in [
            "stale-access",
            "refresh-1",
            "fresh-access",
            "refresh-2",
            "corrupt auth",
        ] {
            assert!(!error.contains(secret), "{error}");
        }
        assert_eq!(
            std::fs::read_to_string(config::auth_path().unwrap()).unwrap(),
            "{ corrupt auth"
        );
        assert_eq!(refreshes(), 1);
        assert!(sent().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_store_after_resolution_is_not_overwritten() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let resolved = resolve(&session).unwrap();
        let path = config::auth_path().unwrap();
        std::fs::write(&path, "{ corrupt auth").unwrap();
        let message = ask(&resolved).await;
        assert_eq!(message.stop_reason, rpi_ai::StopReason::Error);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{ corrupt auth");
        assert_eq!(refreshes(), 0);
        assert!(sent().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credential_refreshed_elsewhere_is_reused() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let resolved = resolve(&session).unwrap();
        // Another rpi process, or `rpi auth refresh`, renewed the stored credential.
        config::upsert_oauth_token(
            "acme",
            "other-access".into(),
            "refresh-9".into(),
            FRESH_EXPIRY,
        )
        .unwrap();

        ask(&resolved).await;
        assert_eq!(refreshes(), 0);
        assert_eq!(sent(), [Some("other-access".to_owned())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_refresh_ends_the_request_without_sending_the_stale_token() {
        let _env = TestEnv::new();
        let session = session(true);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        let resolved = resolve(&session).unwrap();
        FAIL_REFRESH.store(true, SeqCst);

        let message = ask(&resolved).await;
        assert_eq!(message.stop_reason, rpi_ai::StopReason::Error);
        let error = message.error_message.unwrap_or_default();
        assert!(
            error.contains("provider package returned status 1"),
            "{error}"
        );
        assert!(error.contains("rpi auth login --provider acme"), "{error}");
        assert!(
            !error.contains("stale-access")
                && !error.contains("refresh-1")
                && !error.contains("malicious-secret"),
            "{error}"
        );
        assert!(sent().is_empty(), "the stale token is never sent");
        assert_eq!(stored(), ("stale-access".into(), "refresh-1".into(), 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_an_oauth_flow_an_expired_credential_stays_unavailable() {
        let _env = TestEnv::new();
        let session = session(false);
        config::upsert_oauth_token("acme", "stale-access".into(), "refresh-1".into(), 1).unwrap();
        assert!(matches!(
            resolve(&session),
            Err(ResolveError::NoApiKey { .. })
        ));
    }
}
