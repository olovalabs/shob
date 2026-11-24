use super::*;

fn eval(lines: &[&str], foreground: &[&str], focused: bool) -> SessionStatus {
    let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let fg: Vec<String> = foreground.iter().map(|s| s.to_string()).collect();
    let input = StatusInput {
        lines: &lines,
        title: None,
        foreground: &fg,
        detector_state: None,
        osc_progress: "",
    };
    evaluate(&input, SessionStatus::Idle, focused)
}

/// A claude prompt box: two horizontal rules bracketing the input line.
const CLAUDE_IDLE_BOX: &[&str] = &[
    "──────────────────────────────────",
    "  some conversation",
    "──────────────────────────────────",
    "❯ ",
    "──────────────────────────────────",
    "  ? for shortcuts",
];

#[test]
fn tail_lines_keeps_the_last_non_empty_lines() {
    assert_eq!(tail_lines("a\nb\n\nc\n", 2), "b\nc");
    assert_eq!(tail_lines("only", 5), "only");
    assert_eq!(tail_lines("", 3), "");
    assert_eq!(tail_lines("a\nb\nc", 0), "");
    // Trailing whitespace is trimmed, blank lines are skipped.
    assert_eq!(tail_lines("one  \n\ntwo\t\n", 5), "one\ntwo");
}

#[test]
fn priority_order() {
    assert!(SessionStatus::Error.rank() > SessionStatus::Blocked.rank());
    assert!(SessionStatus::Unknown.rank() > SessionStatus::Active.rank());
    assert!(SessionStatus::Done.rank() > SessionStatus::Unknown.rank());
    assert!(SessionStatus::Blocked.rank() > SessionStatus::Done.rank());
    assert!(SessionStatus::Done.rank() > SessionStatus::Working.rank());
    assert!(SessionStatus::Working.rank() > SessionStatus::Active.rank());
    assert!(SessionStatus::Active.rank() > SessionStatus::Idle.rank());
}

#[test]
fn plain_shell_command_is_working() {
    // Process presence alone cannot prove that an unfamiliar command is active.
    assert_eq!(
        eval(&["building..."], &["cargo"], false),
        SessionStatus::Unknown
    );
}

#[test]
fn shell_prompt_is_idle_without_foreground_process() {
    assert_eq!(
        eval(&["user@host ~/project ❯"], &[], false),
        SessionStatus::Idle
    );
    assert_eq!(eval(&[], &[], false), SessionStatus::Idle);
}

#[test]
fn claude_at_its_prompt_is_idle_even_though_the_process_runs() {
    // The pack wins over the foreground fallback: an agent sitting at its
    // prompt is Idle even though its TUI is the running process.
    assert_eq!(
        eval(CLAUDE_IDLE_BOX, &["claude"], false),
        SessionStatus::Idle
    );
}

#[test]
fn claude_working_footer_is_working() {
    assert_eq!(
        eval(
            &[
                "⏺ Read(src/main.rs)",
                "✳ Baking… (2m 14s · esc to interrupt)"
            ],
            &["claude"],
            false
        ),
        SessionStatus::Working
    );
}

#[test]
fn claude_permission_menu_is_blocked() {
    assert_eq!(
        eval(
            &[
                "──────────────────────────────────",
                " Do you want to proceed?",
                " ❯ 1. Yes",
                "   2. No, and tell Claude what to do differently",
                "──────────────────────────────────",
                " esc to cancel · enter to confirm",
            ],
            &["claude"],
            false
        ),
        SessionStatus::Blocked
    );
}

#[test]
fn agent_transcript_errors_do_not_paint_the_terminal_red() {
    // The code being discussed probably contains `error:`; that is not the
    // terminal failing, so the plain-shell Error rule stays out of the way
    // whenever a pack is active.
    assert_ne!(
        eval(
            &["error: could not compile `shob`", "❯ "],
            &["claude"],
            false
        ),
        SessionStatus::Error
    );
}

#[test]
fn gemini_working_hint_is_not_a_blocker() {
    assert_eq!(
        eval(&[" ⠋ Thinking... (esc to cancel)"], &["gemini"], false),
        SessionStatus::Working
    );
}

#[test]
fn agent_running_with_no_matching_rule_is_unknown() {
    // A process being present does not reveal whether the agent is waiting
    // for input or has completed its turn.
    assert_eq!(eval(&["hello"], &["claude"], false), SessionStatus::Unknown);
}

#[test]
fn plain_shell_error_output_is_error() {
    assert_eq!(
        eval(&["bash: foo: command not found"], &[], false),
        SessionStatus::Error
    );
    assert_eq!(
        eval(&["fatal: not a git repository"], &[], false),
        SessionStatus::Error
    );
    // The shell redraws its prompt under the failure; the error must still
    // be visible in the window.
    assert_eq!(
        eval(
            &["error: could not compile `shob`", "user@host ~/shob ❯"],
            &[],
            false
        ),
        SessionStatus::Error
    );
}

#[test]
fn plain_shell_confirmation_prompt_is_blocked() {
    assert_eq!(
        eval(&["Overwrite the file? [y/n]"], &["cp"], false),
        SessionStatus::Blocked
    );
}

#[test]
fn screen_evaluator_does_not_mask_fresh_state_after_completion() {
    let lines = vec!["$".to_string()];
    let input = StatusInput {
        lines: &lines,
        title: None,
        foreground: &[],
        detector_state: None,
        osc_progress: "",
    };
    assert_eq!(
        evaluate(&input, SessionStatus::Done, false),
        SessionStatus::Idle
    );
    assert_eq!(
        evaluate(&input, SessionStatus::Done, true),
        SessionStatus::Active
    );
    assert_eq!(
        evaluate(&input, SessionStatus::Error, false),
        SessionStatus::Idle
    );
}

#[test]
fn active_wins_when_nothing_else() {
    assert_eq!(eval(&["$"], &[], true), SessionStatus::Active);
    assert_eq!(eval(&["$"], &[], false), SessionStatus::Idle);
}

#[test]
fn blocked_beats_active() {
    assert_eq!(
        eval(&["Delete everything? [y/n]"], &["rm"], true),
        SessionStatus::Blocked
    );
}

#[test]
fn agent_names_come_from_the_packs() {
    assert!(is_agent_process("claude"));
    assert!(is_agent_process("claude-code"));
    assert!(is_agent_process("/usr/local/bin/codex"));
    assert!(is_agent_process("gemini"));
    assert!(is_agent_process("opencode"));
    assert!(!is_agent_process("claudette"));
    assert!(!is_agent_process("bash"));
    assert!(!is_agent_process("sleep"));
}

#[test]
fn explain_names_the_matching_rule() {
    let lines: Vec<String> = ["✳ Baking… (2m 14s · esc to interrupt)"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let fg = vec!["claude".to_string()];
    let input = StatusInput {
        lines: &lines,
        title: None,
        foreground: &fg,
        detector_state: None,
        osc_progress: "",
    };
    let m = explain(&input).expect("a rule should match");
    assert_eq!(m.pack, "claude");
    assert_eq!(m.state.label(), "working");
    assert!(m.pack_version.is_some());
}

#[test]
fn pack_lookup_reports_the_agent() {
    assert_eq!(pack_for(&["opencode".to_string()]), Some("opencode"));
    assert_eq!(pack_for(&["bash".to_string()]), None);
}

#[test]
fn missing_process_metadata_is_marked_unknown() {
    assert_eq!(
        unknown_foreground_names().as_ref(),
        &["<process-state-unavailable>"]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn process_names_refresh_when_children_change_in_same_group() {
    let mut cache = HashMap::new();
    let first = cache_foreground_names(
        &mut cache,
        41,
        72,
        vec![(41, Some(1)), (72, Some(2))],
        vec!["bash".into()],
    );
    let second = cache_foreground_names(
        &mut cache,
        41,
        72,
        vec![(41, Some(1)), (72, Some(2)), (73, Some(3))],
        vec!["agent".into(), "node".into()],
    );
    assert_eq!(first.as_ref(), &["bash"]);
    assert_eq!(second.as_ref(), &["agent", "node"]);
    assert!(!Arc::ptr_eq(&first, &second));
}

#[cfg(target_os = "linux")]
#[test]
fn tpgid_is_field_8_not_pgrp() {
    // Real line captured from the `portable-pty` fish shell this app spawns.
    // Field 5 (`pgrp`) equals the pid, which is why reading field 5 always
    // looked like "the shell is at a prompt"; field 8 (`tpgid`) is 380572,
    // the actual foreground job.
    let stat = "380159 (fish) S 379762 380159 380159 34822 380572 4194304 1445 \
                    5045 8 9 4 3 1 2 16 -4 1 0 5676308 300417024 2121 18446744073709551615";
    assert_eq!(parse_stat_tpgid(stat), Some(380572));
}

#[cfg(target_os = "linux")]
#[test]
fn process_instance_start_time_is_part_of_foreground_identity() {
    let mut cache = HashMap::new();
    let first = cache_foreground_names(
        &mut cache,
        41,
        72,
        vec![(72, Some(100))],
        vec!["agent".into()],
    );
    let replacement = cache_foreground_names(
        &mut cache,
        41,
        72,
        vec![(72, Some(200))],
        vec!["agent".into()],
    );
    assert!(!Arc::ptr_eq(&first, &replacement));
}

#[cfg(target_os = "linux")]
#[test]
fn tpgid_parses_comm_with_spaces_and_parens() {
    // `comm` is user-controlled and may contain spaces and `)`, so the split
    // has to happen at the *last* paren, not the first.
    let stat = "42 (my (weird) comm) S 1 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 0 0 7";
    assert_eq!(parse_stat_tpgid(stat), Some(-1));
}
fn feed(d: &mut Detector, screen: &str, t0: Instant, ms: u64) -> State {
    d.update(screen, false, t0 + Duration::from_millis(ms))
}

#[test]
fn running_then_idle() {
    let t0 = Instant::now();
    let mut d = Detector::new(Profile::generic(), t0);
    assert_eq!(
        feed(&mut d, "⠋ Thinking…\n  esc to interrupt", t0, 0),
        State::Running
    );
    // A static generic spinner stops being evidence; silence cannot prove done.
    assert_eq!(
        feed(&mut d, "⠋ Thinking…\n  esc to interrupt", t0, 5000),
        State::Idle
    );
    // done: plain prompt, then it stays unchanged
    assert_eq!(feed(&mut d, "All done.\n> ", t0, 5100), State::Changing);
    assert_eq!(feed(&mut d, "All done.\n> ", t0, 7000), State::Idle);
}

#[test]
fn fresh_shell_output_is_not_agent_work() {
    let t0 = Instant::now();
    let mut detector = Detector::new(Profile::generic(), t0);
    for (screen, ms) in [("restored output", 0), ("restored output\n$ ", 500)] {
        let state = feed(&mut detector, screen, t0, ms);
        let lines = screen.lines().map(str::to_owned).collect::<Vec<_>>();
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &[],
            detector_state: Some(state),
            osc_progress: "",
        };
        assert_eq!(
            evaluate(&input, SessionStatus::Idle, false),
            SessionStatus::Idle
        );
    }
}

#[test]
fn stale_generic_spinner_expires_even_when_unrelated_output_changes() {
    let t0 = Instant::now();
    let mut d = Detector::new(Profile::generic(), t0);
    assert_eq!(feed(&mut d, "⠋ Thinking…\nclock: 0", t0, 0), State::Running);
    let screen = "⠋ Thinking…\nAll done.\nclock: 6\n> ";
    let state = feed(&mut d, screen, t0, 6000);
    assert_eq!(state, State::Idle);
    let lines = screen.lines().map(str::to_owned).collect::<Vec<_>>();
    let foreground = vec!["command-code".to_owned()];
    let input = StatusInput {
        lines: &lines,
        title: None,
        foreground: &foreground,
        detector_state: Some(state),
        osc_progress: "",
    };
    // Do not re-enable the expired spinner through a second regex path,
    // and do not mistake the expiration for a completed turn.
    assert_eq!(
        evaluate(&input, SessionStatus::Working, false),
        SessionStatus::Unknown
    );
}

#[test]
fn changing_spinner_refreshes_generic_working_evidence() {
    let t0 = Instant::now();
    let mut d = Detector::new(Profile::generic(), t0);
    for ms in [0, 4000, 8000, 12000] {
        let screen = if ms % 8000 == 0 {
            "⠋ Thinking…"
        } else {
            "⠙ Thinking…"
        };
        assert_eq!(feed(&mut d, screen, t0, ms), State::Running);
    }
    assert_eq!(feed(&mut d, "⠙ Thinking…", t0, 18000), State::Idle);
}

#[test]
fn authoritative_agent_rule_survives_generic_evidence_expiration() {
    let lines = vec!["✳ Baking… (2m 14s · esc to interrupt)".to_owned()];
    let foreground = vec!["claude".to_owned()];
    let input = StatusInput {
        lines: &lines,
        title: None,
        foreground: &foreground,
        detector_state: Some(State::Idle),
        osc_progress: "",
    };
    assert_eq!(
        evaluate(&input, SessionStatus::Working, false),
        SessionStatus::Working
    );
}

#[test]
fn approval_needs_settled_screen() {
    let t0 = Instant::now();
    let mut d = Detector::new(Profile::generic(), t0);
    let s = "Run `rm -rf build`?\n❯ 1. Yes\n  2. No";
    assert_eq!(feed(&mut d, s, t0, 0), State::Changing); // just appeared
    assert_eq!(feed(&mut d, s, t0, 600), State::AwaitingApproval);
    assert_eq!(feed(&mut d, "Continue? (y/n)", t0, 700), State::Changing);
    assert_eq!(
        feed(&mut d, "Continue? (y/n)", t0, 1200),
        State::AwaitingApproval
    );
}

#[test]
fn exit_and_custom_pattern() {
    let t0 = Instant::now();
    let p = Profile::generic().with_approval(r"trust this folder");
    let mut d = Detector::new(p, t0);
    let s = "Do you trust this folder?";
    feed(&mut d, s, t0, 0);
    assert_eq!(feed(&mut d, s, t0, 600), State::AwaitingApproval);
    assert_eq!(d.update(s, true, t0), State::Exited);
}

#[test]
fn generic_agent_approval_patterns_are_blocked() {
    assert_eq!(
        eval(
            &["Do you want to proceed?", "❯ 1. Yes", "  2. No"],
            &[],
            false
        ),
        SessionStatus::Blocked
    );
    assert_eq!(
        eval(
            &["Allow tool execution `git commit`? (y/n)"],
            &["python3"],
            false
        ),
        SessionStatus::Blocked
    );
    assert_eq!(
        eval(&["Allow dangerous command? [y/n/a/d]"], &[], false),
        SessionStatus::Blocked
    );
    assert_eq!(
        eval(&["Press enter to continue"], &["aider"], false),
        SessionStatus::Blocked
    );
    assert_eq!(
        eval(&["Do you want to edit file? esc to cancel"], &[], false),
        SessionStatus::Blocked
    );
}

#[test]
fn generic_agent_running_markers_are_working() {
    assert_eq!(eval(&["⠋ Thinking…"], &[], false), SessionStatus::Working);
    assert_eq!(
        eval(&["Generating response... (esc to interrupt)"], &[], false),
        SessionStatus::Working
    );
    assert_eq!(
        eval(&["◐ Processing request…"], &[], false),
        SessionStatus::Working
    );
}

#[test]
fn detector_state_reflected_in_evaluate() {
    let lines = vec!["All done.\n> ".to_string()];
    let input_running = StatusInput {
        lines: &lines,
        title: None,
        foreground: &[],
        detector_state: Some(State::Running),
        osc_progress: "",
    };
    assert_eq!(
        evaluate(&input_running, SessionStatus::Idle, false),
        SessionStatus::Working
    );

    let input_approval = StatusInput {
        lines: &lines,
        title: None,
        foreground: &[],
        detector_state: Some(State::AwaitingApproval),
        osc_progress: "",
    };
    assert_eq!(
        evaluate(&input_approval, SessionStatus::Idle, false),
        SessionStatus::Blocked
    );
}
#[test]
fn focus_does_not_turn_ambiguous_foreground_program_into_active() {
    // A live process with no reliable state stays unknown, even when focused.
    assert_eq!(
        eval(&["building..."], &["cargo"], true),
        SessionStatus::Unknown
    );
    assert_eq!(
        eval(&["user@host ❯ git status"], &["git"], true),
        SessionStatus::Unknown
    );
    assert_eq!(
        eval(&["user@host ❯ echo hello"], &[], true),
        SessionStatus::Active
    );
}

#[test]
fn focused_agent_rule_working_is_still_working() {
    // A real agent pack that reports Working wins even while focused.
    assert_eq!(
        eval(
            &[
                "⏺ Read(src/main.rs)",
                "✳ Baking… (2m 14s · esc to interrupt)"
            ],
            &["claude"],
            true
        ),
        SessionStatus::Working
    );
}

#[test]
fn generic_busy_text_is_active_while_focused_and_working_when_away() {
    // No agent pack: generic busy text is not authoritative. Looking at it
    // reads Active; switching away reads Working.
    let lines = ["⠋ Working... (3s · esc to interrupt)"];
    assert_eq!(eval(&lines, &[], true), SessionStatus::Active);
    assert_eq!(eval(&lines, &[], false), SessionStatus::Working);
}
