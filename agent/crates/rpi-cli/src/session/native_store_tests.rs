use super::*;
use rpi_extensions::RuntimeActionHost;
struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn new() -> Self {
        Self(
            [
                crate::config::CONFIG_DIR_ENV,
                "RPI_FORCE_TUI",
                "RPI_SESSION_ID",
            ]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
        )
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
fn args() -> Args {
    Args {
        print: true,
        experimental_native_store: Some("native".into()),
        no_extensions: true,
        no_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("fixture".into()),
        ..Default::default()
    }
}
fn resolved() -> ResolvedModel {
    crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("fixture-key"),
        None,
    )
    .unwrap()
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for item in std::fs::read_dir(path).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out)
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
async fn assert_routes_denied(host: &dyn RuntimeActionHost, old_id: &str) {
    for result in [
        host.new_session(serde_json::json!({})).await,
        host.fork(serde_json::json!({})).await,
        host.navigate_tree(serde_json::json!({})).await,
        host.switch_session(serde_json::json!({"id":old_id})).await,
    ] {
        assert!(result.unwrap_err().contains("forbids session routing"));
    }
}
#[tokio::test]
async fn native_build_is_fresh_and_routing_policy_survives_both_reload_hosts() {
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("old");
    let _env = crate::config::test_support::SessionDirEnv::new(Some(&old));
    let _restore = Environment::new();
    std::env::remove_var("RPI_FORCE_TUI");
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    use rpi_harness::session::{
        jsonl::{JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions},
        types::SessionStorage,
    };
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: Arc::new(OsExecutionEnv::with_cwd(temp.path().to_owned())),
        sessions_root: old.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let legacy = repo
        .create_typed(&JsonlSessionCreateOptions {
            cwd: temp.path().to_string_lossy().into_owned(),
            id: None,
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    legacy.set_name(Some("legacy sentinel")).await.unwrap();
    let old_id = legacy.metadata().id.clone();
    drop(legacy);
    let before = tree(&old);
    let arguments = args();
    let prepared = prepare_extensions(&arguments, temp.path(), false).unwrap();
    assert_eq!(prepared.native_store, Some(temp.path().join("native")));
    let (harness, _events, context) =
        build_prepared(&resolved(), &arguments, temp.path(), false, prepared)
            .await
            .unwrap();
    let id = harness.session().storage().metadata().id.clone();
    assert!(context.fresh_session_only);
    assert!(temp
        .path()
        .join("native")
        .join(format!("{id}.rpinative"))
        .is_file());
    for step in 0..3 {
        if step > 0 {
            if step == 2 {
                context.action_bridge.lock().unwrap().take();
            }
            assert!(
                !reload_extension_resources(&harness, &context)
                    .await
                    .had_errors
            );
        }
        let host = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_host();
        assert_routes_denied(host.as_ref(), &old_id).await;
        assert_eq!(harness.session().storage().metadata().id, id);
        assert_eq!(
            context.tool_context.session_id().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(std::env::var("RPI_SESSION_ID").unwrap(), id);
        assert_eq!(tree(&old), before);
    }
    assert!(!temp.path().join(".rpi/sessions").exists());
    let (second, _events, _ctx) = build(&resolved(), &arguments, temp.path(), false)
        .await
        .unwrap();
    assert_ne!(second.session().storage().metadata().id, id);
}
#[tokio::test]
async fn native_prepare_build_reject_bypasses_and_stdio_before_artifact_creation() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::test_support::SessionDirEnv::new(None);
    let _restore = Environment::new();
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    std::env::remove_var("RPI_FORCE_TUI");
    let mut arguments = args();
    arguments.continue_session = true;
    assert!(prepare_extensions(&arguments, temp.path(), false).is_err());
    arguments.continue_session = false;
    let mut prepared = prepare_extensions(&arguments, temp.path(), false).unwrap();
    let mut registry = rpi_extensions::ExtensionRegistry::new();
    assert!(registry.apply_declaration_json(r#"{"ownsStdio":true}"#));
    prepared.extension_session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        Arc::new(rpi_extensions::PluginKeepalive::new(vec![], None)),
        vec![],
        None,
    );
    assert!(matches!(
        build_prepared(&resolved(), &arguments, temp.path(), false, prepared).await,
        Err(BuildError::NativeStorePolicy(_))
    ));
    assert!(!temp.path().join("native").exists());
    let prepared = prepare_extensions(&arguments, temp.path(), false).unwrap();
    arguments.experimental_native_store = None;
    assert!(
        build_prepared(&resolved(), &arguments, temp.path(), false, prepared)
            .await
            .is_err()
    );
    arguments = args();
    std::env::set_var("RPI_FORCE_TUI", "1");
    assert!(prepare_extensions(&arguments, temp.path(), false).is_err());
    std::env::remove_var("RPI_FORCE_TUI");
    std::fs::write(temp.path().join("native"), "file sentinel").unwrap();
    assert!(prepare_extensions(&arguments, temp.path(), false).is_err());
    assert_eq!(
        std::fs::read(temp.path().join("native")).unwrap(),
        b"file sentinel"
    );
    assert!(!temp.path().join(".rpi/sessions").exists());
}
#[path = "native_store_tests/guards.rs"]
mod guards;
