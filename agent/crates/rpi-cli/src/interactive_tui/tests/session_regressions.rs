use super::*;
use rpi_tui::terminal::{InputEvent, Terminal, TerminalInfo};
use std::sync::atomic::{AtomicUsize, Ordering};

fn state_with_root(root: &std::path::Path) -> Arc<TuiState> {
    let mut state = test_tui_state();
    Arc::get_mut(&mut state).unwrap().session_dir = root.to_path_buf();
    state
}

fn assert_session_locked(path: &str) {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{path}.lock"))
        .unwrap();
    assert!(
        fs2::FileExt::try_lock_exclusive(&lock).is_err(),
        "{path} must remain exclusively locked"
    );
}

/// An empty authenticated catalog: these tests exercise session plumbing, not
/// model resolution, so `restore_session_model` finds nothing to apply and
/// leaves the lane untouched.
fn empty_catalog() -> Arc<std::sync::RwLock<Vec<rpi_ai::Model>>> {
    Arc::new(std::sync::RwLock::new(Vec::new()))
}

#[tokio::test]
async fn tui_fork_locks_the_copy_and_keeps_the_selected_root_for_switch_and_picker() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    source
        .append_message(user_message_with_images("custom-root history", vec![]))
        .await
        .unwrap();
    let initial_id = source.storage().metadata().id.clone();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    fork_session(&harness, &cwd, &state.chat_container, &state, &context).await;
    let fork_id = harness.session().storage().metadata().id.clone();
    assert_ne!(fork_id, initial_id);
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 2);
    for row in &metadata {
        assert_session_locked(&row.path);
    }
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &initial_id,
            &cwd,
            &state.chat_container,
            &state,
            &context,
            &empty_catalog()
        )
        .await
    );
    assert_eq!(harness.session().storage().metadata().id, initial_id);
    let rendered = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(rendered.contains("custom-root history"));

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let editor_container = Arc::new(Container::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    open_session_selector(&state, &editor_container, &state.editor, &tui, &tx);
    assert!(
        state.selector_open(),
        "picker must find sessions in the selected root"
    );
    let router = state
        .active_selector
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .0
        .clone();
    router.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(TuiMessage::SwitchSession(selected)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
    else {
        panic!("session selection expected")
    };
    assert!(metadata.iter().any(|row| row.path.contains(&selected)));
    assert!(!cwd.join(".rpi/sessions").exists());
}

#[tokio::test]
async fn tui_import_preserves_identity_history_and_source_across_cwds() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    let old_cwd = temporary.path().join("original project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&old_cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source_root = temporary.path().join("source");
    let source = crate::session::create_jsonl_session(&source_root, &old_cwd.to_string_lossy())
        .await
        .unwrap();
    source.set_name(Some("Imported history")).await.unwrap();
    source
        .append_message(user_message_with_images(
            "preserved imported prompt",
            vec![],
        ))
        .await
        .unwrap();
    let expected = source.storage().metadata().id.clone();
    let source_path = crate::session::list_session_files_sync(&source_root).remove(0);
    let original = std::fs::read(&source_path).unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &source_path.to_string_lossy(),
        &cwd,
        &state.chat_container,
        &state,
        &context,
        &empty_catalog(),
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, expected);
    assert_eq!(
        harness.session().get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    assert!(strip_ansi(&state.chat_container.render(160).join("\n"))
        .contains("preserved imported prompt"));
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].cwd, old_cwd.to_string_lossy());
    assert_session_locked(&metadata[0].path);
    let restored =
        crate::session::open_session_by_id_in_dir(&expected, &cwd.to_string_lossy(), &root)
            .await
            .map_err(|error| error.to_string())
            .unwrap()
            .session;
    assert_eq!(
        restored.get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    let imported = std::fs::read(&metadata[0].path).unwrap();
    assert_eq!(imported, original);
    // Both repeated imports and importing a repository's own log must fail
    // without touching either saved history or the original source.
    for path in [&source_path, &std::path::PathBuf::from(&metadata[0].path)] {
        assert!(
            crate::session::import_session_file(path, &cwd.to_string_lossy(), &root)
                .await
                .err()
                .unwrap()
                .contains("already exists")
        );
    }
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    assert_eq!(std::fs::read(&metadata[0].path).unwrap(), imported);
    assert_eq!(crate::session::list_session_files_sync(&root).len(), 1);
}

#[tokio::test]
async fn invalid_import_leaves_the_active_session_and_repository_untouched() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("selected");
    let invalid = cwd.join("invalid.jsonl");
    std::fs::write(&invalid, "not a session\n").unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let initial = harness.session().storage().metadata().id.clone();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &invalid.to_string_lossy(),
        cwd,
        &state.chat_container,
        &state,
        &context,
        &empty_catalog(),
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, initial);
    assert!(crate::session::list_session_files_sync(&root).is_empty());
    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "staging files must be cleaned up"
    );
    assert_eq!(std::fs::read_to_string(invalid).unwrap(), "not a session\n");
}

/// Every path that rebuilds the transcript for another session must advertise
/// the session's REAL capability snapshot.
///
/// The regression this locks: those paths used to call `add_welcome_message`,
/// which passes empty slices, so a session with tools and skills still showed
/// `Tools (0) none` / `Skills (0) none` right after `/session`, `/import`,
/// `/fork` or tree navigation.
#[tokio::test]
async fn session_switch_banner_reports_real_capabilities() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let first = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let first_id = first.storage().metadata().id.clone();
    first
        .append_message(user_message_with_images("first session history", vec![]))
        .await
        .unwrap();
    let second = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let second_id = second.storage().metadata().id.clone();
    assert_ne!(first_id, second_id);

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(second).await.unwrap();
    // One real tool so `Tools (N)` has something to count; the empty-sentinel
    // fallback would otherwise mask a broken banner.
    harness
        .set_tools(
            vec![rpi_harness::types::HarnessTool::new(Arc::new(
                BannerProbeTool::new("probe-tool"),
            ))],
            None,
        )
        .await
        .unwrap();
    harness
        .set_resources(rpi_harness::types::AgentHarnessResources {
            skills: Some(vec![rpi_harness::types::Skill {
                name: "demo-skill".into(),
                description: "a skill the banner must advertise".into(),
                content: String::new(),
                file_path: "/skills/demo-skill/SKILL.md".into(),
                disable_model_invocation: None,
            }]),
            ..Default::default()
        })
        .await
        .unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());

    assert!(
        switch_to_session(
            &harness,
            &lane,
            &first_id,
            cwd,
            &state.chat_container,
            &state,
            &context,
            &empty_catalog()
        )
        .await
    );

    let banner = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(
        banner.contains("Skills (1)"),
        "the banner must count the session's skill: {banner}"
    );
    assert!(
        banner.contains("demo-skill"),
        "the banner must name the session's skill: {banner}"
    );
    // Reverse assertion: the empty-capability rendering must be gone. Without
    // it, a partially-empty implementation could still pass on unrelated text.
    assert!(
        !banner.contains("Skills (0)"),
        "the banner must not fall back to the empty capability state: {banner}"
    );
    assert!(
        !banner.contains("Tools (0) none"),
        "the banner must not report zero tools: {banner}"
    );
    assert!(
        banner.contains("Tools (1)") && banner.contains("probe-tool"),
        "the banner must count the session's tools: {banner}"
    );
}

/// `/new` must start a NEW session file, not keep appending to the old one, and
/// must reset the per-session footer counters while keeping the environment.
#[tokio::test]
async fn new_session_creates_a_file_and_resets_footer() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let previous_id = source.storage().metadata().id.clone();
    source
        .append_message(user_message_with_images(
            "history that stays behind",
            vec![],
        ))
        .await
        .unwrap();

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());

    // Simulate a session that already accumulated usage before `/new`.
    state.footer.set_model("(prov) model");
    state.footer.set_context_window(200_000);
    state.footer.set_session_name(Some("old session"));
    state.footer.add_usage(12_000, 3_400, 40_000, 1_200, 0.421);
    state.footer.set_context_tokens(Some(126_400));

    assert!(
        start_new_session(
            &harness,
            &lane,
            cwd,
            &state.chat_container,
            &state,
            &context
        )
        .await
    );

    // (a) the harness serves a different session
    let new_id = harness.session().storage().metadata().id.clone();
    assert_ne!(new_id, previous_id, "`/new` must swap the session id");
    assert_eq!(
        std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).unwrap_or_default(),
        new_id,
        "the plugin env channel must follow the swap"
    );
    assert_eq!(
        context.session_id().as_deref(),
        Some(new_id.as_str()),
        "the plugin tool context must follow the swap"
    );

    // (b) the new session file really exists under the session root. The JSONL
    // repo nests files under a cwd-derived directory, so walk the tree instead
    // of assuming a flat layout.
    let files = jsonl_files_under(&root);
    assert!(
        files.iter().any(|path| path.contains(&new_id)),
        "new session file missing under {}: {files:?}",
        root.display()
    );
    assert_eq!(
        files.iter().filter(|path| path.contains(&new_id)).count(),
        1,
        "exactly one file may back the new session: {files:?}"
    );
    // The previous session is preserved, not deleted: `/new` must not destroy
    // user data.
    assert!(
        files.iter().any(|path| path.contains(&previous_id)),
        "the previous session file must survive: {files:?}"
    );

    // (c) the footer's session-scoped numbers are gone, the environment stays.
    let stats = strip_ansi(&state.footer.render(120).join("\n"));
    assert!(!stats.contains("↑12k"), "usage must reset: {stats}");
    assert!(!stats.contains("$0.421"), "cost must reset: {stats}");
    assert!(!stats.contains("old session"), "name must reset: {stats}");
    assert!(
        !stats.contains("63.2%"),
        "context badge must reset: {stats}"
    );
    assert!(stats.contains("(prov) model"), "model kept: {stats}");
    assert!(stats.contains("200k"), "window kept: {stats}");

    // The transcript is the new (empty) session, not the old history.
    let banner = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(
        !banner.contains("history that stays behind"),
        "the old transcript must not follow into the new session: {banner}"
    );
    assert!(
        banner.contains(&format!("Started new session {new_id}.")),
        "the note must name the new session: {banner}"
    );
}

/// Every `.jsonl` file under `root`, at any depth.
fn jsonl_files_under(root: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl")
                && !path.to_string_lossy().ends_with(".progress.jsonl")
            {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found
}

/// A fresh in-memory session identical in shape to the one `--no-session`
/// builds (`session.rs`'s `ephemeral_session`), so the regression below
/// exercises the real persistence policy, not a hand-picked double.
fn ephemeral_harness_session() -> rpi_harness::session::Session {
    let storage = rpi_harness::session::memory::InMemorySessionStorage::new(
        rpi_harness::session::types::SessionMetadata {
            id: "ephemeral".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(rpi_harness::session::memory::SystemClock),
        Arc::new(rpi_harness::session::session::DefaultIdGenerator::new()),
    );
    rpi_harness::session::Session::new(Arc::new(storage), None)
}

// Local review probes for PR #98 (session-boundary regressions), kept as
// regression tests. They drive the real `start_new_session` swap path with
// the same in-memory storage `--no-session` installs; no network, real
// filesystem confined to a tempdir.

#[tokio::test]
async fn review_pr98_new_keeps_an_ephemeral_session_off_disk() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness
        .set_session(ephemeral_harness_session())
        .await
        .unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    assert!(!root.exists());
    assert!(
        start_new_session(
            &harness,
            &lane,
            cwd,
            &state.chat_container,
            &state,
            &context
        )
        .await
    );
    lane.append_message(user_message_with_images(
        "review-only ephemeral message",
        vec![],
    ))
    .await
    .unwrap();
    let files = jsonl_files_under(cwd);
    assert!(
        files.is_empty(),
        "--no-session must remain in memory after /new; persisted files: {files:?}"
    );
}

#[tokio::test]
async fn review_pr98_new_must_not_switch_under_an_unscoped_user_bash_run() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let previous_id = source.storage().metadata().id.clone();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    let cancellation = state.begin_user_bash();
    assert!(*state.status.lock().unwrap() == RunStatus::Idle);
    let switched = start_new_session(
        &harness,
        &lane,
        cwd,
        &state.chat_container,
        &state,
        &context,
    )
    .await;
    // UserBashFinished has no originating-session field and appends through
    // this same live lane. It must not straddle a session swap uncoordinated.
    assert!(
        !switched
            || cancellation.is_cancelled()
            || harness.session().storage().metadata().id == previous_id,
        "/new switched while the previous session's unscoped !command remained active"
    );
}

// --- Session C (P4 + P5): model replay + footer usage restoration ----------

/// An assistant turn with the given provider/model and usage, shaped like the
/// entries a real session records (stop `Stop`, usage filled in).
fn recorded_assistant_turn(
    provider: &str,
    model: &str,
    usage: rpi_ai::types::Usage,
) -> AgentMessage {
    let mut a = rpi_ai::AssistantMessage::empty(
        rpi_ai::Api::Faux,
        provider,
        model,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default(),
    );
    a.stop_reason = rpi_ai::StopReason::Stop;
    a.usage = usage;
    AgentMessage::Assistant(Box::new(a))
}

/// Switching back to a session must put the lane on THAT session's model: the
/// session log (its `ModelChange`/assistant entries) is the authority, not
/// whatever `/model` last picked in the session the user is leaving.
#[tokio::test]
async fn switching_back_to_a_session_restores_its_model() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let first = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let first_id = first.storage().metadata().id.clone();
    // The first session's assistant turn names the model it ran on.
    first
        .append_message(recorded_assistant_turn(
            "faux",
            "second-model",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    let second = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let second_id = second.storage().metadata().id.clone();
    assert_ne!(first_id, second_id);

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(second).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // The user's last `/model` pick (in the session being left) was `first`.
    lane.set_model(rpi_ai::Model::new(
        "first-model",
        "first",
        rpi_ai::Api::Faux,
        "faux",
        "http://x",
    ))
    .await
    .unwrap();

    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    // The authenticated catalog carries the session's recorded model.
    let catalog: Arc<std::sync::RwLock<Vec<rpi_ai::Model>>> =
        Arc::new(std::sync::RwLock::new(vec![
            rpi_ai::Model::new(
                "first-model",
                "first",
                rpi_ai::Api::Faux,
                "faux",
                "http://x",
            ),
            rpi_ai::Model::new(
                "second-model",
                "second",
                rpi_ai::Api::Faux,
                "faux",
                "http://x",
            ),
        ]));

    assert!(
        switch_to_session(
            &harness,
            &lane,
            &first_id,
            cwd,
            &state.chat_container,
            &state,
            &context,
            &catalog
        )
        .await
    );

    let model = lane.get_model().await.unwrap();
    assert_eq!(
        model.id, "second-model",
        "the lane must run the resumed session's own model"
    );
    let footer = strip_ansi(&state.footer.render(120).join("\n"));
    assert!(
        footer.contains("(faux) second-model"),
        "the footer must show the resumed session's model: {footer}"
    );
}

/// Switching back to a session must restore THAT session's usage totals,
/// cache-hit rate and context badge — neither blank nor the previous
/// session's numbers.
#[tokio::test]
async fn switching_back_to_a_session_restores_its_footer_usage() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let first = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let first_id = first.storage().metadata().id.clone();
    // Two recorded turns: 12k in / 3.4k out with heavy cache reads, then a
    // turn establishing the final context size.
    first
        .append_message(user_message_with_images("history prompt", vec![]))
        .await
        .unwrap();
    first
        .append_message(recorded_assistant_turn(
            "faux",
            "m",
            rpi_ai::types::Usage {
                input: 12_000,
                output: 3_400,
                cache_read: 40_000,
                cache_write: 1_200,
                ..rpi_ai::types::Usage::zero()
            },
        ))
        .await
        .unwrap();
    first
        .append_message(recorded_assistant_turn(
            "faux",
            "m",
            rpi_ai::types::Usage {
                input: 6_000,
                output: 1_000,
                cache_read: 20_000,
                cache_write: 0,
                total_tokens: 126_400,
                ..rpi_ai::types::Usage::zero()
            },
        ))
        .await
        .unwrap();
    let second = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let second_id = second.storage().metadata().id.clone();
    assert_ne!(first_id, second_id);

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(second).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // The session being left had accumulated DIFFERENT live counters.
    let state = state_with_root(&root);
    state.footer.set_context_window(200_000);
    state.footer.add_usage(111_111, 22_222, 0, 0, 9.999);

    assert!(
        switch_to_session(
            &harness,
            &lane,
            &first_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &empty_catalog()
        )
        .await
    );

    let stats = strip_ansi(&state.footer.render(120).join("\n"));
    // The first session's restored totals (12k + 6k in, 3.4k + 1k out), not
    // the numbers the previous session left behind…
    assert!(stats.contains("↑18k"), "input totals restored: {stats}");
    assert!(stats.contains("↓4.4k"), "output totals restored: {stats}");
    assert!(
        !stats.contains("↑111k"),
        "stale totals must be gone: {stats}"
    );
    // …and its latest turn's cache-hit share (20k read / 26k billed), under
    // the same latest-turn semantics as the live `MessageEnd` handler.
    assert!(stats.contains("76.9%"), "cache hit rate restored: {stats}");
    // The last recorded context size, against the live window.
    assert!(stats.contains("63.2%"), "context badge restored: {stats}");
}

/// The replay is best-effort by contract: a session whose recorded model is no
/// longer in the authenticated catalog must still open, leaving the current
/// lane model untouched instead of failing the switch.
#[tokio::test]
async fn unknown_session_model_leaves_the_current_model_in_place() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let first = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let first_id = first.storage().metadata().id.clone();
    // The session was last driven by a model that has since disappeared from
    // the catalog.
    first
        .append_message(recorded_assistant_turn(
            "faux",
            "retired-model",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    let second = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let second_id = second.storage().metadata().id.clone();
    assert_ne!(first_id, second_id);

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(second).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let before = lane.get_model().await.unwrap();

    let state = state_with_root(&root);
    // Catalog carries only an unrelated model.
    let catalog: Arc<std::sync::RwLock<Vec<rpi_ai::Model>>> =
        Arc::new(std::sync::RwLock::new(vec![rpi_ai::Model::new(
            "first-model",
            "first",
            rpi_ai::Api::Faux,
            "faux",
            "http://x",
        )]));

    assert!(
        switch_to_session(
            &harness,
            &lane,
            &first_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &catalog
        )
        .await,
        "switching must succeed even when the recorded model is unavailable"
    );
    let after = lane.get_model().await.unwrap();
    assert_eq!(after.id, before.id, "the lane model must stay untouched");
    assert_eq!(
        harness.session().storage().metadata().id,
        first_id,
        "the switch itself must have completed"
    );
}

fn pr104_review_model(id: &str) -> rpi_ai::Model {
    rpi_ai::Model::new(id, id, rpi_ai::Api::Faux, "faux", "http://x")
}

#[tokio::test]
async fn review_pr104_replay_preserves_the_live_cache_hit_rate() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let source_id = source.storage().metadata().id.clone();
    let messages = [
        recorded_assistant_turn(
            "faux",
            "m",
            rpi_ai::types::Usage {
                input: 100,
                output: 1,
                cache_read: 900,
                ..rpi_ai::types::Usage::zero()
            },
        ),
        recorded_assistant_turn(
            "faux",
            "m",
            rpi_ai::types::Usage {
                input: 900,
                output: 1,
                cache_read: 100,
                ..rpi_ai::types::Usage::zero()
            },
        ),
    ];
    for message in &messages {
        source.append_message(message.clone()).await.unwrap();
    }
    let target = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(target).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    // Run the actual live MessageEnd handler; the latest turn is a 10% hit.
    for message in messages {
        handle_agent_event(
            AgentEvent::MessageEnd {
                message_id: None,
                message,
            },
            &tui,
            &state,
            &state.chat_container,
            &lane,
        )
        .await;
    }
    let before = strip_ansi(&state.footer.render(120).join("\n"));
    assert!(before.contains("CH10.0%"), "live hit rate: {before}");
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &source_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &empty_catalog(),
        )
        .await
    );
    let after = strip_ansi(&state.footer.render(120).join("\n"));
    assert!(
        after.contains("CH10.0%"),
        "restoring the same two turns must preserve the live rate, not average all turns: {after}"
    );
}

#[tokio::test]
async fn review_pr104_restore_model_must_follow_the_selected_branch() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let source_id = source.storage().metadata().id.clone();
    let selected = source
        .append_message(recorded_assistant_turn(
            "faux",
            "selected",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    source
        .append_message(recorded_assistant_turn(
            "faux",
            "abandoned",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    // This is the move made by /tree before leaving/reopening the session.
    source.move_lane("main", Some(&selected)).await.unwrap();
    let target = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(target).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    lane.set_model(pr104_review_model("selected"))
        .await
        .unwrap();
    let state = state_with_root(&root);
    let catalog = Arc::new(std::sync::RwLock::new(vec![
        pr104_review_model("selected"),
        pr104_review_model("abandoned"),
    ]));
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &source_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &catalog,
        )
        .await
    );
    assert_eq!(
        lane.get_tip_id().await.unwrap().as_deref(),
        Some(selected.as_str())
    );
    assert_eq!(
        lane.get_model().await.unwrap().id,
        "selected",
        "a later entry outside the selected branch must not select the model"
    );
}

#[tokio::test]
async fn review_pr104_restore_model_must_ignore_a_child_lane() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let source_id = source.storage().metadata().id.clone();
    let parent = source
        .append_message(recorded_assistant_turn(
            "faux",
            "parent-model",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    source
        .create_lane("child-review", Some(&parent))
        .await
        .unwrap();
    source
        .view("child-review")
        .append_message(recorded_assistant_turn(
            "faux",
            "child-model",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    let target = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(target).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    lane.set_model(pr104_review_model("parent-model"))
        .await
        .unwrap();
    let state = state_with_root(&root);
    let catalog = Arc::new(std::sync::RwLock::new(vec![
        pr104_review_model("parent-model"),
        pr104_review_model("child-model"),
    ]));
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &source_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &catalog,
        )
        .await
    );
    assert_eq!(
        lane.get_model().await.unwrap().id,
        "parent-model",
        "a child lane's later assistant turn must not replace the parent's model"
    );
}

#[tokio::test]
async fn review_pr104_fork_preserves_a_model_pick_before_the_next_turn() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    source
        .append_message(recorded_assistant_turn(
            "faux",
            "old-model",
            rpi_ai::types::Usage::zero(),
        ))
        .await
        .unwrap();
    let previous_id = source.storage().metadata().id.clone();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    // The real /model path does not persist settings or a ModelChange entry.
    apply_model_selection(&lane, &state, pr104_review_model("new-model"), false)
        .await
        .unwrap();
    fork_session(
        &harness,
        cwd,
        &state.chat_container,
        &state,
        &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
    )
    .await;
    assert_ne!(harness.session().storage().metadata().id, previous_id);
    assert_eq!(
        lane.get_model().await.unwrap().id,
        "new-model",
        "/fork must not undo a successful /model selection made before the next assistant turn"
    );
}

fn review_084_context_turn(model: &str, tokens: i64) -> AgentMessage {
    recorded_assistant_turn(
        "faux",
        model,
        rpi_ai::types::Usage {
            input: tokens,
            output: 1,
            total_tokens: tokens,
            ..rpi_ai::types::Usage::zero()
        },
    )
}

#[tokio::test]
async fn rereview_pr104_context_must_not_come_from_a_child_lane() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let source_id = source.storage().metadata().id.clone();
    let main_tip = source
        .append_message(review_084_context_turn("parent-model", 50_000))
        .await
        .unwrap();
    source
        .create_lane("child-context", Some(&main_tip))
        .await
        .unwrap();
    source
        .view("child-context")
        .append_message(review_084_context_turn("child-model", 190_000))
        .await
        .unwrap();
    let target = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(target).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let mut parent_model = pr104_review_model("parent-model");
    parent_model.context_window = 200_000;
    let catalog = Arc::new(std::sync::RwLock::new(vec![parent_model]));
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &source_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &catalog,
        )
        .await
    );
    // The previous model fix works, but usage replay still queries all lanes.
    assert_eq!(lane.get_model().await.unwrap().id, "parent-model");
    let footer = strip_ansi(&state.footer.render(160).join("\n"));
    assert!(
        footer.contains("25.0%/200k"),
        "the main context is 50k/200k, not the child's 190k/200k: {footer}"
    );
}

#[tokio::test]
async fn rereview_pr104_compaction_must_invalidate_pre_compaction_context() {
    use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind};
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let source_id = source.storage().metadata().id.clone();
    source
        .append_message(review_084_context_turn("model", 100_000))
        .await
        .unwrap();
    // Seed the durable state after a successful /compact, without a model call.
    // run_compact() explicitly clears the live context badge for this state.
    source
        .append_entry(
            ProvisionedEntry {
                id: "review-context-compaction".into(),
                kind: ProvisionedKind::Compaction {
                    summary: "Small replacement summary".into(),
                    retained_tail: vec![],
                    tokens_before: 100_000,
                    details: None,
                    usage: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    let target = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(target).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let mut model = pr104_review_model("model");
    model.context_window = 200_000;
    let catalog = Arc::new(std::sync::RwLock::new(vec![model]));
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &source_id,
            cwd,
            &state.chat_container,
            &state,
            &rpi_extensions::ToolCallContext::new(cwd.to_string_lossy()),
            &catalog,
        )
        .await
    );
    let footer = strip_ansi(&state.footer.render(160).join("\n"));
    assert!(
        footer.split_whitespace().any(|part| part == "0.0%/200k"),
        "after compaction there is no new context report; old 50% must not reappear: {footer}"
    );
}

/// A tool that exists only to give the banner something to count.
struct BannerProbeTool {
    schema: rpi_ai::types::Tool,
}

impl BannerProbeTool {
    fn new(name: &str) -> Self {
        Self {
            schema: rpi_ai::types::Tool {
                name: name.to_string(),
                description: "probe tool".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({
                    "type": "object",
                    "properties": {},
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for BannerProbeTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    fn execution_mode(&self) -> rpi_agent::types::ToolExecutionMode {
        rpi_agent::types::ToolExecutionMode::Parallel
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::AgentError> {
        Ok(rpi_agent::types::AgentToolResult::text("probe"))
    }
}

// Local review probes for the footer freeze (user bug list #3). The user
// reported the footer's numbers stopping after a save; the confirmed defect is
// narrower but real: the counters are accumulated only from `MessageEnd`, and
// that event is terminal, so an overflowing `AgentEvent` broadcast drops it
// with nothing later to re-deliver it.

/// An assistant turn with explicit usage, so a footer assertion can tell which
/// turns were counted.
fn assistant_with_usage(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost: f64,
) -> rpi_ai::types::AssistantMessage {
    let mut message = rpi_ai::types::AssistantMessage::empty(
        rpi_ai::Api::AnthropicMessages,
        "probe-provider",
        "probe-model",
        0,
    );
    message.stop_reason = rpi_ai::StopReason::Stop;
    message.usage = rpi_ai::types::Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: rpi_ai::types::UsageCost {
            total: cost,
            ..Default::default()
        },
        ..rpi_ai::types::Usage::zero()
    };
    message
}

/// The rendered footer, ANSI stripped, as one line.
fn footer_text(state: &Arc<TuiState>) -> String {
    rpi_tui::utils::strip_ansi(&state.footer.render(200).join(" "))
}

/// A dropped `MessageEnd` must not freeze the footer for the rest of the
/// session: the end-of-run reconciliation rebuilds every counter from the
/// durable log, which is the same source the replay path reads.
#[tokio::test]
async fn footer_reconcile_rebuilds_totals_after_a_dropped_message_end() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    lane.append_message(user_message_with_images("first", vec![]))
        .await
        .unwrap();
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        1_000, 200, 500, 100, 0.010,
    ))))
    .await
    .unwrap();
    lane.append_message(user_message_with_images("second", vec![]))
        .await
        .unwrap();
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        2_000, 300, 6_000, 0, 0.020,
    ))))
    .await
    .unwrap();

    let state = test_tui_state();
    // Precondition: the footer saw only the FIRST turn's `MessageEnd`, exactly
    // the state a `Lagged` broadcast leaves behind.
    state.footer.set_context_window(200_000);
    state.footer.add_usage(1_000, 200, 500, 100, 0.010);
    let before = footer_text(&state);
    assert!(
        before.contains("↑1.0k"),
        "precondition: only the first turn is counted: {before}"
    );

    reconcile_footer_from_lane(&lane, &state).await;

    let after = footer_text(&state);
    assert!(
        after.contains("↑3.0k"),
        "usage totals must be rebuilt from the log: {after}"
    );
    assert!(
        after.contains("$0.030"),
        "cost must be rebuilt from the log: {after}"
    );
    // The cache-hit rate follows the LATEST eligible turn (6000/8000 = 75.0%),
    // the same rule the live handler and the replay path apply. The first turn
    // alone would read 31.2% and a session average 62.5%, so this assertion
    // pins the semantic rather than just "some rate is present".
    assert!(
        after.contains("CH75.0%"),
        "cache-hit rate must follow the latest turn: {after}"
    );
}

/// Reconciliation replaces the counters instead of adding to them. The live
/// path usually already accumulated the same totals, so an accumulating
/// implementation would double every number on a normal turn.
#[tokio::test]
async fn footer_reconcile_is_idempotent_on_top_of_the_live_totals() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        1_000, 200, 500, 100, 0.010,
    ))))
    .await
    .unwrap();
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        2_000, 300, 6_000, 0, 0.020,
    ))))
    .await
    .unwrap();

    let state = test_tui_state();
    state.footer.set_context_window(200_000);
    // The live drain saw BOTH turns before reconciliation ran.
    state.footer.add_usage(1_000, 200, 500, 100, 0.010);
    state.footer.add_usage(2_000, 300, 6_000, 0, 0.020);

    reconcile_footer_from_lane(&lane, &state).await;
    let once = footer_text(&state);
    reconcile_footer_from_lane(&lane, &state).await;
    let twice = footer_text(&state);

    assert_eq!(once, twice, "reconciliation must not accumulate");
    assert!(
        once.contains("↑3.0k"),
        "the live totals must survive unchanged: {once}"
    );
}

/// A distinct turn whose timestamp and token counts equal a turn the log
/// already holds must still reach the footer. This is the production shape of
/// the collision the maintainer's probe exercises: two assistant turns can
/// report identical timestamps (timestamp 0 from a fixture, or an equal
/// provider timestamp) and identical usage. Reconciliation runs at `AgentEnd`,
/// where the FIFO broadcast guarantees no event of the finished run can arrive
/// after it — so no dedupe key is consulted at all, and every persisted turn
/// the rebuild reads counts.
#[tokio::test]
async fn review_distinct_turn_with_same_timestamp_and_usage_is_not_dropped() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let mut first = assistant_with_usage(1000, 200, 500, 0, 0.01);
    first.content = vec![rpi_ai::types::Content::text("first response")];
    lane.append_message(AgentMessage::Assistant(Box::new(first)))
        .await
        .unwrap();
    let state = test_tui_state();
    reconcile_footer_from_lane(&lane, &state).await;
    let mut second = assistant_with_usage(1000, 200, 500, 0, 0.01);
    second.content = vec![rpi_ai::types::Content::text("different response")];
    lane.append_message(AgentMessage::Assistant(Box::new(second.clone())))
        .await
        .unwrap();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    handle_agent_event(
        AgentEvent::MessageEnd {
            message_id: None,
            message: AgentMessage::Assistant(Box::new(second)),
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;
    let text = footer_text(&state);
    assert!(
        text.contains("↑2.0k") && text.contains("$0.020"),
        "a distinct turn was mistaken for a delayed copy: {text}"
    );
}

/// The maintainer's original double-count scenario, re-expressed for the
/// `AgentEnd` barrier: when reconciliation runs from the drain, every event
/// queued before it has already been applied, so the same turn's delayed
/// `MessageEnd` cannot follow the rebuild. Simulating the exact race (rebuild
/// running while a `MessageEnd` is still queued) requires a second event
/// source, which production no longer has: the only caller is the drain
/// itself. The invariant this probe pins is the one the barrier guarantees —
/// a rebuild followed by a replay of the same events leaves the totals
/// unchanged, because the rebuild replaced them and the replay re-applied the
/// same turns to the same values.
#[tokio::test]
async fn review_reconcile_at_agent_end_is_not_recounted_by_a_later_identical_event() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let message = AgentMessage::Assistant(Box::new(assistant_with_usage(1_000, 200, 500, 0, 0.01)));
    lane.append_message(message.clone()).await.unwrap();
    let state = test_tui_state();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    // The live drain counted the turn first, as it does before `AgentEnd`.
    handle_agent_event(
        AgentEvent::MessageEnd {
            message_id: None,
            message,
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;
    let expected = footer_text(&state);
    // The barrier point: the rebuild replaces the accumulated totals.
    reconcile_footer_from_lane(&lane, &state).await;
    assert_eq!(
        footer_text(&state),
        expected,
        "reconciliation must restore exactly what the live path counted"
    );
    // The next run's turn: same timestamp+usage shape as a persisted turn the
    // rebuild already counted. The old identity ledger would have suppressed
    // this; the barrier makes it a plain, correct accumulation.
    let next_run =
        AgentMessage::Assistant(Box::new(assistant_with_usage(1_000, 200, 500, 0, 0.01)));
    lane.append_message(next_run.clone()).await.unwrap();
    handle_agent_event(
        AgentEvent::MessageEnd {
            message_id: None,
            message: next_run,
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;
    let after = footer_text(&state);
    assert!(
        after.contains("↑2.0k"),
        "a later identical-shaped turn must count on top of a rebuild: {after}"
    );
}

/// The duplicate-suppression invariant, re-stated for the `AgentEnd` barrier:
/// the live drain counts a turn first; the drain's own reconciliation then
/// replaces the totals with the same values; nothing can re-add that turn
/// afterwards, because the barrier guarantees every event queued before the
/// rebuild was already applied and no `MessageEnd` of the finished run arrives
/// after `AgentEnd`. Replay-driven re-entry (`/resume` render) also replaces
/// rather than accumulates. Pinning the "does not double" half of the
/// contract: `review_distinct_turn_...` pins the "distinct turns count" half.
#[tokio::test]
async fn review_pending_message_end_after_reconcile_does_not_double_usage() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let message = AgentMessage::Assistant(Box::new(assistant_with_usage(1_000, 200, 500, 0, 0.01)));
    lane.append_message(message.clone()).await.unwrap();
    let state = test_tui_state();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    // The live drain applied the turn's `MessageEnd` first.
    handle_agent_event(
        AgentEvent::MessageEnd {
            message_id: None,
            message,
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;
    let expected = footer_text(&state);
    // The barrier point: the drain reconciles AFTER everything it delivered.
    // The rebuild must restore the same totals, not add them again.
    reconcile_footer_from_lane(&lane, &state).await;
    assert_eq!(
        footer_text(&state),
        expected,
        "a rebuild after the drain applied every event must not double the usage"
    );
    // The durable log is the authority: a second rebuild (as a session switch
    // or resume would trigger) is idempotent, still without a ledger.
    reconcile_footer_from_lane(&lane, &state).await;
    assert_eq!(
        footer_text(&state),
        expected,
        "repeated rebuilds must stay idempotent"
    );
}

/// The dedupe ledger must only suppress turns reconciliation actually rebuilt
/// from the log. A message whose append was refused never reaches session
/// storage, so reconciliation cannot have counted it — its terminal event is the
/// only chance the user's footer gets to see that usage.
#[tokio::test]
async fn review_unpersisted_message_after_reconcile_still_counts_once() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // Persisted turn: reconciliation sees it.
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        1_000, 200, 500, 0, 0.010,
    ))))
    .await
    .unwrap();
    let state = test_tui_state();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    reconcile_footer_from_lane(&lane, &state).await;
    assert!(
        footer_text(&state).contains("↑1.0k"),
        "precondition: reconciliation rebuilt the persisted turn"
    );

    // A later turn that never made it into storage: same shape, different usage.
    let unpersisted =
        AgentMessage::Assistant(Box::new(assistant_with_usage(4_000, 300, 0, 0, 0.040)));
    handle_agent_event(
        AgentEvent::MessageEnd {
            message_id: None,
            message: unpersisted,
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;
    let footer = footer_text(&state);
    assert!(
        footer.contains("↑5.0k"),
        "an unpersisted turn must still be counted when its event arrives: {footer}"
    );
    assert!(
        footer.contains("$0.050"),
        "its cost must be counted too: {footer}"
    );
}

/// `/name` and a resumed session both have to reach the footer's line 1: the
/// slot is rendered (`pwd_line`) but nothing in production ever wrote it, so
/// the name was recorded in the log and then never shown.
#[tokio::test]
async fn a_sessions_name_reaches_footer_line_one() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_name(Some("named-session")).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = test_tui_state();
    state.footer.set_cwd("/home/u/proj");

    // A replay is the path a session switch and a resume both take.
    render_session_history(
        &harness,
        &state.chat_container,
        None,
        Some(state.extension_session.clone()),
        state.images_visible(),
        Some(&state.footer),
    )
    .await;

    let footer = footer_text(&state);
    assert!(
        footer.contains("named-session"),
        "the recorded name must render on line 1: {footer}"
    );

    // Clearing the name (a `/name` with an empty argument) must clear the slot
    // rather than leave the stale one behind.
    harness.set_name(None).await.unwrap();
    render_session_history(
        &harness,
        &state.chat_container,
        None,
        Some(state.extension_session.clone()),
        state.images_visible(),
        Some(&state.footer),
    )
    .await;
    let cleared = footer_text(&state);
    assert!(
        !cleared.contains("named-session"),
        "a cleared name must leave line 1: {cleared}"
    );

    // `lane` is unused by this probe's assertions but keeps the view wiring
    // honest: the name must come from the lane's own session, not a copy.
    assert_eq!(
        lane.session_view().get_name().await.unwrap(),
        None,
        "the lane must observe the same session the footer was rendered from"
    );
}

/// The defect, expressed with baseline-only API: once a `MessageEnd` is
/// dropped, nothing in the run path re-derives the footer's totals, so the
/// numbers stay at whatever the drain happened to observe.
///
/// This probe PASSES on both the baseline and the fixed head on purpose: it
/// documents the frozen state that `reconcile_footer_from_lane` repairs, and
/// keeps the sibling probes honest about what "fixed" means. It is NOT a
/// fail-on-old probe — `footer_reconcile_rebuilds_totals_after_a_dropped_message_end`
/// is the one that breaks if reconciliation is removed.
#[tokio::test]
async fn a_dropped_message_end_leaves_the_footer_frozen_without_reconciliation() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        1_000, 200, 500, 100, 0.010,
    ))))
    .await
    .unwrap();
    lane.append_message(AgentMessage::Assistant(Box::new(assistant_with_usage(
        2_000, 300, 6_000, 0, 0.020,
    ))))
    .await
    .unwrap();

    let state = test_tui_state();
    state.footer.set_context_window(200_000);
    // The drain observed only the first turn's `MessageEnd`.
    state.footer.add_usage(1_000, 200, 500, 100, 0.010);
    // The baseline's only end-of-run fallback: the badge, from the run outcome.
    let last = assistant_with_usage(2_000, 300, 6_000, 0, 0.020);
    record_context_usage(&state.footer, &last);

    let frozen = footer_text(&state);
    // The badge moves (it has a fallback), the totals do not — exactly the
    // partial freeze the user described.
    assert!(
        frozen.contains("↑1.0k") && !frozen.contains("↑3.0k"),
        "precondition: totals stay at the dropped-event value: {frozen}"
    );
    // The cache-hit rate is worse off than "stale": `set_cache_hit_rate` is
    // called only from the dropped `MessageEnd`, so it never appears at all.
    assert!(
        !frozen.contains("CH"),
        "precondition: the cache-hit rate never got set: {frozen}"
    );
    // The badge DID update, because it has its own fallback. That asymmetry is
    // why the freeze reads as "only some of the footer stopped".
    assert!(
        frozen.contains("4.2%/200k"),
        "precondition: the badge DID update, so the freeze looks partial: {frozen}"
    );
}

/// The TUI tells the user to `/export` a refused session write. That advice is
/// only honest if the export really carries the refused message, so this pins
/// the claim the guidance makes rather than just its wording: it drives the
/// production export and asserts the file contains the unsaved text.
#[tokio::test]
async fn review_the_export_advice_recovers_the_unsaved_message() {
    use rpi_ai::types::UserContent;

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let text = "only on screen";
    let refused = rpi_harness::messages::create_custom_message(
        "probe",
        UserContent::Text(text.into()),
        true,
        None,
        0,
    );
    harness.keep_unsaved_message_for_test("a message you sent", refused);
    assert_eq!(
        harness.unsaved_messages().len(),
        1,
        "precondition: the harness retained the refused message"
    );

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("export.md");
    crate::export::export_session(&harness, crate::export::ExportFormat::Markdown, &out)
        .await
        .unwrap();
    let written = std::fs::read_to_string(&out).unwrap();
    assert!(
        written.contains(text),
        "/export must write the refused message it recommends exporting: {written}"
    );

    let notice = tui_session_storage_notice(&rpi_harness::result::HarnessError::session_storage(
        "disk full",
    ));
    assert!(notice.contains("/export"), "{notice}");
}

struct RenderCountingTerminal(Arc<AtomicUsize>);
impl Terminal for RenderCountingTerminal {
    fn info(&self) -> TerminalInfo {
        TerminalInfo {
            columns: 80,
            rows: 24,
            ..Default::default()
        }
    }
    fn write(&self, text: &str) {
        // A full content repaint emits a screen clear (tui_alt_screen's
        // full_redraw path); diff patches do not.
        if text.contains("\x1b[2J") {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _: usize, _: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn start(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>, _: Box<dyn Fn() + Send + Sync>) {}
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        false
    }
    fn set_progress(&self, _: bool) {}
    fn flush(&self) {}
}

// review_pr202: a background theme reload (success or parse error) must
// schedule a content repaint. Neither appending a note nor the palette change
// requests a frame, and the scheduler only repaints an idle session when
// size/status content actually changes — so without the explicit request the
// reload result stays invisible until the next keypress.
#[test]
fn review_pr202_theme_watch_notify_repaints_idle_session() {
    let frames = Arc::new(AtomicUsize::new(0));
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(RenderCountingTerminal(frames.clone())),
        false,
        None,
    ));
    tui.set_layout_root(Some(Arc::new(rpi_tui::Text::new("idle", 0, 0))));
    tui.start_readerless();
    let before = frames.load(Ordering::SeqCst);

    let state = test_tui_state();
    // Both callback outcomes must leave the old palette behind.
    let notify_ok = theme_watch_notify(&state, tui.clone());
    notify_ok(Ok("Theme reloaded.".into()));
    let after_ok = frames.load(Ordering::SeqCst);
    assert!(after_ok > before, "success notice must schedule a repaint");

    let notify_err = theme_watch_notify(&state, tui.clone());
    notify_err(Err("cannot load theme: bad value".into()));
    assert!(
        frames.load(Ordering::SeqCst) > after_ok,
        "error notice must schedule a repaint too"
    );
}
