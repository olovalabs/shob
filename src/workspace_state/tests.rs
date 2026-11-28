use super::*;

#[test]
fn legacy_chat_defaults_to_presets_without_an_effort_override() {
    let chat: SavedChat = serde_json::from_str(r#"{"fast_mode":false}"#).unwrap();
    assert!(!chat.fast_mode);
    assert!(chat.thinking_override.is_none());
}

#[test]
fn clearing_workspace_fences_older_layout_and_scrollback_writes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace.json");
    let scrollbacks = dir.path().join("sessions");
    let generation = AtomicU64::new(1);
    let state = SavedWorkspace::default();
    assert!(save_if_current(&workspace, &state, &generation, 1).unwrap());
    write_scrollback_if_current_in(&scrollbacks, "pane", "old", &generation, 1).unwrap();

    generation.store(2, Ordering::SeqCst);
    remove_saved_workspace(&workspace, &scrollbacks).unwrap();
    assert!(!save_if_current(&workspace, &state, &generation, 1).unwrap());
    write_scrollback_if_current_in(&scrollbacks, "pane", "old", &generation, 1).unwrap();
    assert!(!workspace.exists());
    assert!(!scrollbacks.exists());
}

const LEGACY: &str = r#"{
        "projects": [
            {"path": "/tmp/one", "name": "one", "expanded": true},
            {"path": "/tmp/two", "name": "", "expanded": false}
        ],
        "active_project": "/tmp/two",
        "sidebar_visible": false,
        "sidebar_width": 300.0,
        "theme": "dark",
        "palette_scheme": "github",
        "bottom_input": true
    }"#;

#[test]
fn legacy_workspace_migrates_to_one_session_per_project() {
    let workspace = load_from_text(LEGACY);
    assert_eq!(workspace.version, CURRENT_VERSION);
    assert_eq!(workspace.projects.len(), 2);
    assert_eq!(workspace.active_project, Some(PathBuf::from("/tmp/two")));

    let first = &workspace.projects[0];
    assert_eq!(first.path, PathBuf::from("/tmp/one"));
    assert_eq!(first.sessions.len(), 1);
    let session = &first.sessions[0];
    assert_eq!(session.panes.len(), 1);
    assert_eq!(
        session.tree,
        SavedPaneNode::Leaf {
            pane: session.panes[0].id
        }
    );
    assert_eq!(session.panes[0].cwd, Some(PathBuf::from("/tmp/one")));

    let second = &workspace.projects[1];
    assert_eq!(second.name, "two");
    assert!(!second.expanded);
}

#[test]
fn full_workspace_roundtrips() {
    let workspace = SavedWorkspace {
        version: CURRENT_VERSION,
        active_project: Some(PathBuf::from("/tmp/one")),
        projects: vec![SavedProject {
            id: 1,
            path: PathBuf::from("/tmp/one"),
            name: "one".into(),
            expanded: true,
            active_session: 0,
            sessions: vec![SavedSession {
                id: 2,
                uid: "s-1".into(),
                title: "agent".into(),
                display_title: "agent".into(),
                logo: "shob".into(),
                maximized_pane_id: Some(5),
                focused_pane: Some(4),
                tree: SavedPaneNode::Split {
                    id: 6,
                    axis: SavedAxis::Vertical,
                    left: Box::new(SavedPaneNode::Leaf { pane: 4 }),
                    right: Box::new(SavedPaneNode::Leaf { pane: 5 }),
                },
                panes: vec![
                    SavedPane {
                        id: 4,
                        uid: "p-1".into(),
                        title: "zsh".into(),
                        display_title: "zsh".into(),
                        cwd: Some(PathBuf::from("/tmp/one/src")),
                        draft: "cargo t".into(),
                        queued_messages: vec!["First".into(), "Second".into()],
                        command_history: vec!["cargo test".into()],
                        chat_active: true,
                        chat: SavedChat {
                            fast_mode: false,
                            thinking_override: Some("max".into()),
                            thinking_by_model: Default::default(),
                            selected_model: Some(AgentModel {
                                provider: "xai-oauth".into(),
                                id: "grok-4.7".into(),
                                backend: crate::agent_auth::ModelBackend::Shob,
                            }),
                            first_response_provider: Some("anthropic".into()),
                            model_explicit: true,
                            agent_title: None,
                            prompt_title: None,
                            shob_session_id: Some("session-1".into()),
                            conversation_key: None,
                            workspace_override: None,
                            models: vec![SavedModelChat {
                                model: Some(AgentModel {
                                    provider: "commandcode".into(),
                                    id: "deepseek/deepseek-v4-flash".into(),
                                    backend: crate::agent_auth::ModelBackend::Shob,
                                }),
                                shob_session_id: None,
                                conversation: "omp_commandcode_deepseek_deepseek-v4-flash".into(),
                                agent_title: Some("🧠 DEEP: Wire the flash model".into()),
                                prompt_title: None,
                            }],
                        },
                        ..Default::default()
                    },
                    SavedPane {
                        id: 5,
                        uid: "p-2".into(),
                        title: "vim".into(),
                        display_title: "vim".into(),
                        ..Default::default()
                    },
                ],
            }],
        }],
        threads: Vec::new(),
    };
    let text = serde_json::to_string(&workspace).unwrap();
    let restored = load_from_text(&text);
    assert_eq!(restored, workspace);
    let restored_chat: SavedChat = serde_json::from_value(serde_json::json!({
        "shob_session_id": "existing-session",
        "selected_model": {"provider": "anthropic", "id": "model", "backend": "LegacyName"}
    }))
    .unwrap();
    assert_eq!(
        restored_chat.shob_session_id.as_deref(),
        Some("existing-session")
    );
    assert_eq!(
        restored_chat.selected_model.unwrap().backend,
        crate::agent_auth::ModelBackend::Embedded
    );
}

#[test]
fn explicit_automatic_model_survives_a_round_trip() {
    // `None` alone is ambiguous: never-picked vs deliberately "Automatic
    // (shob)". The flag is what distinguishes them after a restart.
    let workspace = SavedWorkspace {
        version: CURRENT_VERSION,
        active_project: Some(PathBuf::from("/tmp/one")),
        projects: vec![SavedProject {
            id: 1,
            path: PathBuf::from("/tmp/one"),
            name: "one".into(),
            expanded: true,
            active_session: 0,
            sessions: vec![SavedSession {
                id: 2,
                uid: "s-1".into(),
                title: "agent".into(),
                display_title: "agent".into(),
                logo: String::new(),
                maximized_pane_id: None,
                focused_pane: Some(4),
                tree: SavedPaneNode::Leaf { pane: 4 },
                panes: vec![SavedPane {
                    id: 4,
                    uid: "p-1".into(),
                    title: "zsh".into(),
                    display_title: "zsh".into(),
                    chat_active: true,
                    chat: SavedChat {
                        model_explicit: true,
                        ..Default::default()
                    },
                    ..Default::default()
                }],
            }],
        }],
        threads: Vec::new(),
    };
    let text = serde_json::to_string(&workspace).unwrap();
    let restored = load_from_text(&text);
    let chat = &restored.projects[0].sessions[0].panes[0].chat;
    assert_eq!(chat.selected_model, None);
    assert!(
        chat.model_explicit,
        "an explicit Automatic must stay explicit"
    );
}

#[test]
fn dangling_tree_leaves_are_dropped() {
    let text = r#"{
            "version": 2,
            "projects": [{
                "id": 1, "path": "/tmp/p", "name": "p",
                "sessions": [{
                    "id": 2, "uid": "s-1",
                    "tree": {"type":"split","id":9,"axis":"horizontal",
                        "left":{"type":"leaf","pane":42},
                        "right":{"type":"leaf","pane":43}},
                    "panes": [{"id":43,"uid":"p-1","title":"t"}]
                }]
            }]
        }"#;
    let workspace = load_from_text(text);
    let session = &workspace.projects[0].sessions[0];
    assert_eq!(session.tree, SavedPaneNode::Leaf { pane: 43 });
    assert_eq!(session.focused_pane, Some(43));
}

#[test]
fn malformed_workspace_falls_back_to_default() {
    assert_eq!(load_from_text("not json"), SavedWorkspace::default());
    let empty = load_from_text("{}");
    assert_eq!(empty.version, CURRENT_VERSION);
    assert!(empty.projects.is_empty());
}

#[test]
fn load_repairs_duplicate_and_out_of_range_ids_without_losing_drafts() {
    let huge = usize::MAX;
    let text = serde_json::json!({
        "version": 2,
        "projects": [
            {"id": 1, "path": "/first", "sessions": [{
                "id": 2, "panes": [{"id": 3}], "tree": {"type": "leaf", "pane": 3}
            }]},
            {"id": 1, "path": "/second", "sessions": [{
                "id": 2, "focused_pane": 3, "maximized_pane_id": huge,
                "panes": [{"id": 3, "draft": "keep me", "tool_panel_tab": huge},
                          {"id": huge, "tool_panel_owner": 3}],
                "tree": {"type": "split", "id": huge,
                         "left": {"type": "leaf", "pane": 3},
                         "right": {"type": "leaf", "pane": huge}}
            }]}
        ]
    });
    let saved = load_from_text(&text.to_string());
    let first = &saved.projects[0];
    let second = &saved.projects[1];
    assert_ne!(first.id, second.id);
    assert_ne!(first.sessions[0].id, second.sessions[0].id);
    let session = &second.sessions[0];
    let [pane, panel] = session.panes.as_slice() else {
        panic!("lost a pane")
    };
    assert_ne!(pane.id, first.sessions[0].panes[0].id);
    assert_eq!(pane.draft, "keep me");
    assert_eq!(pane.tool_panel_tab, Some(panel.id));
    assert_eq!(panel.tool_panel_owner, Some(pane.id));
    assert_eq!(session.focused_pane, Some(pane.id));
    assert_eq!(session.maximized_pane_id, Some(panel.id));
    let SavedPaneNode::Split { left, right, .. } = &session.tree else {
        panic!("lost split")
    };
    assert_eq!(**left, SavedPaneNode::Leaf { pane: pane.id });
    assert_eq!(**right, SavedPaneNode::Leaf { pane: panel.id });
    assert!(saved.max_id() < 10_000);
}

#[test]
fn scrollback_cap_keeps_the_newest_complete_lines() {
    let text = "one\ntwo\nthree\nfour\n";
    assert_eq!(cap_scrollback(text, text.len()), text);
    assert_eq!(cap_scrollback(text, 9), "four\n");
    assert_eq!(cap_scrollback(text, 1), "");
}

#[test]
fn scrollback_cap_respects_utf8_boundaries() {
    let text = "αααα\nββββ\n";
    let capped = cap_scrollback(text, 6);
    assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
    assert!(capped.is_empty() || capped.ends_with('\n'));
    assert!(capped.len() <= text.len());
}

#[test]
fn max_id_includes_split_nodes() {
    let mut workspace = SavedWorkspace {
        projects: vec![SavedProject {
            id: 1,
            sessions: vec![SavedSession {
                id: 2,
                tree: SavedPaneNode::Split {
                    id: 99,
                    axis: SavedAxis::Horizontal,
                    left: Box::new(SavedPaneNode::Leaf { pane: 4 }),
                    right: Box::new(SavedPaneNode::Leaf { pane: 5 }),
                },
                panes: vec![SavedPane {
                    id: 4,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    workspace.threads = Vec::new();
    assert_eq!(workspace.max_id(), 99);
}

#[test]
fn command_history_is_bounded() {
    let history: Vec<String> = (0..(MAX_COMMAND_HISTORY + 5))
        .map(|index| index.to_string())
        .collect();
    let text = serde_json::json!({
        "version": 2,
        "projects": [{
            "path": "/tmp/p", "name": "p",
            "sessions": [{
                "uid": "s", "tree": {"type":"leaf","pane":1},
                "panes": [{"id":1,"uid":"p","title":"t","command_history":history}]
            }]
        }]
    })
    .to_string();
    let workspace = load_from_text(&text);
    let pane = &workspace.projects[0].sessions[0].panes[0];
    assert_eq!(pane.command_history.len(), MAX_COMMAND_HISTORY);
    assert_eq!(pane.command_history[0], "5");
}

#[test]
fn atomic_save_and_load_roundtrip() {
    let path = std::env::temp_dir().join(format!("shob-ws-{}.json", paths::uid("t")));
    let workspace = SavedWorkspace {
        version: CURRENT_VERSION,
        active_project: Some(PathBuf::from("/tmp/one")),
        projects: vec![SavedProject {
            id: 1,
            path: PathBuf::from("/tmp/one"),
            name: "one".into(),
            expanded: true,
            active_session: 0,
            sessions: vec![SavedSession {
                id: 2,
                uid: "s-1".into(),
                title: "shell".into(),
                display_title: "shell".into(),
                logo: "shob".into(),
                focused_pane: Some(3),
                tree: SavedPaneNode::Leaf { pane: 3 },
                panes: vec![SavedPane {
                    id: 3,
                    uid: "p-1".into(),
                    title: "zsh".into(),
                    display_title: "zsh".into(),
                    cwd: Some(PathBuf::from("/tmp/one/src")),
                    draft: "cargo t".into(),
                    command_history: vec!["cargo test".into()],
                    chat: SavedChat {
                        workspace_override: Some(PathBuf::from("/tmp/one-wt")),
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }],
        threads: Vec::new(),
    };
    save_atomic(&path, &workspace).expect("save");
    let loaded = load(&path);
    assert_eq!(loaded, workspace);
    let _ = fs::remove_file(path);
}

#[test]
fn scrollback_store_roundtrips_and_prunes() {
    let dir = std::env::temp_dir().join(paths::uid("scrollback-test"));
    write_scrollback_in(&dir, "p-1", "hello\r\n").unwrap();
    assert_eq!(
        read_scrollback_in(&dir, "p-1").as_deref(),
        Some("hello\r\n")
    );
    write_scrollback_in(&dir, "p-2", "world\r\n").unwrap();
    let mut live = HashSet::new();
    live.insert("p-1".to_string());
    prune_stale_scrollbacks_in(&dir, &live);
    assert!(read_scrollback_in(&dir, "p-1").is_some());
    assert!(read_scrollback_in(&dir, "p-2").is_none());

    write_scrollback_in(&dir, "p-1", "   ").unwrap();
    assert!(read_scrollback_in(&dir, "p-1").is_none());
    let _ = fs::remove_dir_all(dir);
}

fn workspace_with_session(session_uid: &str) -> SavedWorkspace {
    SavedWorkspace {
        version: CURRENT_VERSION,
        active_project: Some(PathBuf::from("/tmp/one")),
        projects: vec![SavedProject {
            id: 1,
            path: PathBuf::from("/tmp/one"),
            name: "one".into(),
            sessions: vec![SavedSession {
                id: 2,
                uid: session_uid.into(),
                tree: SavedPaneNode::Leaf { pane: 3 },
                panes: vec![SavedPane {
                    id: 3,
                    uid: "p-3".into(),
                    title: "terminal".into(),
                    display_title: "terminal".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
        threads: Vec::new(),
    }
}

#[test]
fn sidebar_thread_metadata_roundtrips() {
    let mut workspace = workspace_with_session("thread-1");
    workspace.threads = vec![SavedSidebarThread {
        session_uid: "thread-1".into(),
        pinned: true,
        pin_order: Some(1),
        settled: false,
        snoozed_until_ms: Some(42),
        archived: false,
        parked_at_ms: Some(7),
        title: Some("Planning notes".into()),
    }];
    workspace.normalize();
    let text = serde_json::to_string(&workspace).unwrap();
    let restored = load_from_text(&text);
    assert_eq!(restored, workspace);
    assert_eq!(restored.threads[0].session_uid, "thread-1");
    assert_eq!(restored.threads[0].pin_order, Some(1));
}

#[test]
fn sidebar_thread_metadata_defaults_and_dedupes() {
    let legacy: SavedWorkspace = serde_json::from_str(r#"{"version":2,"projects":[]}"#).unwrap();
    assert!(legacy.threads.is_empty());

    let mut workspace = workspace_with_session("same");
    workspace.threads = vec![
        SavedSidebarThread {
            session_uid: "same".into(),
            pinned: true,
            ..Default::default()
        },
        SavedSidebarThread {
            session_uid: "same".into(),
            settled: true,
            ..Default::default()
        },
        SavedSidebarThread {
            session_uid: "".into(),
            archived: true,
            ..Default::default()
        },
    ];
    let restored = load_from_text(&serde_json::to_string(&workspace).unwrap());
    assert_eq!(restored.threads.len(), 1);
    assert_eq!(restored.threads[0].session_uid, "same");
    assert!(restored.threads[0].pinned);
    assert!(restored.threads[0].pin_order.is_some());
    assert!(!restored.threads[0].settled);
}
