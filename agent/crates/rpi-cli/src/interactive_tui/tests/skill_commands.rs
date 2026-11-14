use super::*;
use rpi_harness::types::{AgentHarnessResources, PromptTemplate, Skill};

fn resources(name: &str) -> AgentHarnessResources {
    AgentHarnessResources {
        skills: Some(vec![Skill {
            name: name.into(),
            description: "Explicit skill invocation".into(),
            content: "body".into(),
            file_path: format!("/skills/{name}/SKILL.md"),
            disable_model_invocation: Some(true),
        }]),
        prompt_templates: Some(vec![PromptTemplate {
            name: "summarize".into(),
            description: Some("Summarize the diff".into()),
            content: "template".into(),
        }]),
        ..Default::default()
    }
}

fn suggestions(manager: &AutocompleteManager, input: &str) -> Vec<String> {
    manager
        .get_suggestions(input, input.len())
        .map(|s| s.items.into_iter().map(|i| i.text).collect())
        .unwrap_or_default()
}

#[test]
fn hidden_skills_use_pi_syntax_and_enter_accepts_production_completion() {
    let state = test_tui_state();
    configure_resource_autocomplete(
        &state.autocomplete,
        build_builtin_registry().visible_entries(),
        &resources("manual"),
        std::path::Path::new("."),
        true,
    );
    assert!(suggestions(&state.autocomplete, "/skill:man").contains(&"/skill:manual".into()));
    assert!(suggestions(&state.autocomplete, "/summ").contains(&"/summarize".into()));
    assert!(suggestions(&state.autocomplete, "/usag").contains(&"/usage".into()));
    state.editor.set_text("/skill:man");
    state.editor.set_cursor(0, 10);
    let completion = resolve_enter_completion(&state, &state.editor).unwrap();
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &state.editor));
    assert_eq!(state.editor.get_text(), "/skill:manual");
    // Skill commands deliberately use ordinary prompt/queue/image handling,
    // not the legacy /skills command's local run path.
    assert!(build_builtin_registry().find("/skill:manual").is_none());
}

#[test]
fn skill_command_setting_defaults_true_and_project_false_overrides_global_true() {
    let defaults = crate::settings::Settings::default();
    assert!(skill_commands_enabled(&defaults, &[]));
    let enabled = crate::settings::Settings {
        enable_skill_commands: Some(true),
        ..Default::default()
    };
    let disabled = crate::settings::Settings {
        enable_skill_commands: Some(false),
        ..Default::default()
    };
    assert!(!skill_commands_enabled(&enabled, &[disabled.clone()]));
    assert!(skill_commands_enabled(&disabled, &[enabled]));
    let manager = AutocompleteManager::new();
    configure_resource_autocomplete(
        &manager,
        build_builtin_registry().visible_entries(),
        &resources("manual"),
        std::path::Path::new("."),
        false,
    );
    assert!(!suggestions(&manager, "/skill:man").contains(&"/skill:manual".into()));
    assert!(suggestions(&manager, "/usag").contains(&"/usage".into()));
}

#[test]
fn rebuilding_autocomplete_removes_stale_skills_and_adds_reloaded_names() {
    let manager = AutocompleteManager::new();
    for name in ["oldskill", "freshskill"] {
        configure_resource_autocomplete(
            &manager,
            build_builtin_registry().visible_entries(),
            &resources(name),
            std::path::Path::new("."),
            true,
        );
        let items = suggestions(&manager, "/skill:");
        assert!(items.contains(&format!("/skill:{name}")));
        if name == "freshskill" {
            assert!(!items.contains(&"/skill:oldskill".into()));
        }
    }
    configure_resource_autocomplete(
        &manager,
        build_builtin_registry().visible_entries(),
        &Default::default(),
        std::path::Path::new("."),
        true,
    );
    assert!(!suggestions(&manager, "/skill:")
        .iter()
        .any(|s| s.starts_with("/skill:")));
}

#[test]
fn resource_autocomplete_preserves_priority_reservations_and_default_descriptions() {
    let manager = AutocompleteManager::new();
    let mut commands = build_builtin_registry().visible_entries();
    commands.push(SlashCommandEntry {
        name: "/skill:extension".into(),
        description: "Extension wins".into(),
    });
    let mut loaded = resources("manual");
    let mut duplicate = loaded.skills.as_ref().unwrap()[0].clone();
    duplicate.description = "Duplicate must lose".into();
    let mut extension = duplicate.clone();
    extension.name = "extension".into();
    loaded
        .skills
        .as_mut()
        .unwrap()
        .extend([duplicate, extension]);
    loaded.prompt_templates = Some(
        [
            "help",
            "q",
            "skill:extension",
            "skill:manual",
            "summarize",
            "/summarize",
        ]
        .into_iter()
        .map(|name| PromptTemplate {
            name: name.into(),
            description: None,
            content: "template".into(),
        })
        .collect(),
    );
    configure_resource_autocomplete(&manager, commands, &loaded, std::path::Path::new("."), true);
    let items = manager.get_suggestions("/", 1).unwrap().items;
    let find = |name: &str| items.iter().find(|item| item.text == name).unwrap();
    assert_eq!(
        find("/help").description.as_deref(),
        Some("Show available commands")
    );
    assert_eq!(
        find("/skill:extension").description.as_deref(),
        Some("Extension wins")
    );
    assert_eq!(
        find("/skill:manual").description.as_deref(),
        Some("Expand prompt template")
    );
    assert_eq!(
        find("/summarize").description.as_deref(),
        Some("Expand prompt template")
    );
    assert!(!items.iter().any(|item| item.text == "/q"));
    let unique: std::collections::BTreeSet<_> = items.iter().map(|item| &item.text).collect();
    assert_eq!(unique.len(), items.len());
}
