use super::*;

fn screen_input(screen: &str) -> DetectionInput<'_> {
    DetectionInput {
        screen,
        osc_title: "",
        osc_progress: "",
    }
}

#[test]
fn every_embedded_pack_parses_and_compiles() {
    let set = embedded();
    // 22 agent packs ship in assets/agent-detection/.
    assert_eq!(set.packs.len(), 22, "expected all vendored packs to load");
    assert!(set.packs.iter().all(|pack| !pack.rules.is_empty()));
    // The packs we care most about must be present under their real ids.
    for id in ["claude", "codex", "gemini", "opencode", "cursor", "copilot"] {
        assert!(
            set.packs.iter().any(|pack| pack.id == id),
            "missing pack {id}"
        );
    }
}

#[test]
fn pack_names_cover_ids_aliases_and_file_stems() {
    let set = embedded();
    // id
    assert_eq!(set.identify("claude"), Some("claude"));
    // alias
    assert_eq!(set.identify("claude-code"), Some("claude"));
    // file stem, where it differs from the id
    assert_eq!(set.identify("antigravity"), Some("agy"));
    assert_eq!(set.identify("github-copilot"), Some("copilot"));
    // path-qualified argv0
    assert_eq!(set.identify("/usr/local/bin/opencode"), Some("opencode"));
    // not agents
    assert_eq!(set.identify("fish"), None);
    assert_eq!(set.identify("sleep"), None);
}

#[test]
fn regions_slice_the_way_upstream_does() {
    let screen = "one\n\ntwo\nthree\nfour";
    assert_eq!(region(screen_input(screen), "whole_recent"), screen);
    assert_eq!(
        region(screen_input(screen), "bottom_lines(2)"),
        "three\nfour"
    );
    assert_eq!(
        region(screen_input(screen), "bottom_non_empty_lines(2)"),
        "three\nfour"
    );
    assert_eq!(
        region(screen_input(screen), "bottom_non_empty_lines(4)"),
        screen
    );
    // Runs through to the end of the second non-empty line, blank lines
    // and all — matching upstream's offset arithmetic.
    assert_eq!(
        region(screen_input(screen), "top_non_empty_lines(2)"),
        "one\n\ntwo\n"
    );
    // OSC specs read their own field, never the screen.
    let input = DetectionInput {
        screen,
        osc_title: "⠋ Working",
        osc_progress: "4;0",
    };
    assert_eq!(region(input, "osc_title"), "⠋ Working");
    assert_eq!(region(input, "osc_progress"), "4;0");
    // Unknown specs are inert rather than matching everything.
    assert_eq!(region(screen_input(screen), "nonsense_region"), "");
}

#[test]
fn claude_live_footer_is_working_not_blocked() {
    // `esc to interrupt` with an activity bullet is claude's live turn.
    let screen = "⏺ Read(src/main.rs)\n✳ Baking… (2m 14s · esc to interrupt)";
    let pack = embedded()
        .pack_for(&["claude".to_string()])
        .expect("claude pack");
    let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
    assert_eq!(m.state, RuleState::Working, "matched {}", m.rule);
}

#[test]
fn claude_permission_menu_is_blocked() {
    let screen = "\
────────────────────────────
 Do you want to proceed?
 ❯ 1. Yes
   2. No, and tell Claude what to do differently
────────────────────────────
 esc to cancel · enter to confirm";
    let pack = embedded()
        .pack_for(&["claude".to_string()])
        .expect("claude pack");
    let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
    assert_eq!(m.state, RuleState::Blocked, "matched {}", m.rule);
}

#[test]
fn gemini_working_hint_is_not_a_blocker() {
    // gemini ships `esc to cancel` as its *working* hint; the pack encodes
    // that directly, which is why the packs beat hand-rolled rules.
    let screen = " ⠋ Thinking... (esc to cancel)";
    let pack = embedded()
        .pack_for(&["gemini".to_string()])
        .expect("gemini pack");
    let m = evaluate(pack, screen_input(screen)).expect("a rule should match");
    assert_eq!(m.state, RuleState::Working, "matched {}", m.rule);
}

#[test]
fn unknown_region_makes_a_rule_inert() {
    // A rule pointing at a region we do not implement must never match.
    let pack = RulePack::parse(
        r#"
id = "test"
[[rules]]
id = "bogus"
state = "blocked"
priority = 100
region = "made_up_region"
contains = ["x"]
"#,
        "agent-detection/test.toml",
        "test",
    )
    .expect("pack parses");
    assert!(evaluate(&pack, screen_input("x")).is_none());
}
