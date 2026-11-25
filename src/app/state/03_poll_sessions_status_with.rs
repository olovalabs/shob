impl AppState {
    /// Poll every pane against a foreground-process snapshot gathered off the
    /// UI thread. The snapshot is keyed by shell pid; a pane whose pid is
    /// missing simply sees an empty foreground group for this tick.
    pub fn poll_sessions_status_with(
        &mut self,
        foregrounds: &HashMap<u32, Arc<[String]>>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(unix)]
        self.apply_status_events(status_bus::drain(), cx);
        let diag_poll = diagnostics::poll_start();
        let mut changed = false;
        let mut title_changed = false;
        let mut panes_polled = 0u64;
        let focused_id = self.focused_pane_id;
        let agent_layout = self.settings.agent_layout;
        // `T3_STATUS_DEBUG=1` dumps every detection pass: the facts that went in
        // and the badge that came out. Without this the poller is invisible.
        let debug = status::debug_enabled();
        let trace = status::trace_enabled();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                let default_primary_id = if agent_layout {
                    session
                        .panes
                        .iter()
                        .find(|pane| pane.one_mode && !pane.tool_panel && pane.chat.active)
                        .map(|pane| pane.id)
                        .or_else(|| session.pane_tree.as_ref().map(|tree| tree.first_leaf()))
                } else {
                    session.pane_tree.as_ref().map(|tree| tree.first_leaf())
                };
                let pane_count = session.panes.len();
                let mut primary_idx: Option<usize> = None;
                let mut primary_focused = false;
                let mut primary_fg: Arc<[String]> = status::empty_foreground_names();

                for (pane_idx, pane) in session.panes.iter_mut().enumerate() {
                    panes_polled += 1;
                    let focused = focused_id == Some(pane.id);
                    // Reuse the same immutable foreground snapshot for status,
                    // cache keys, and the session logo (one /proc probe per pane,
                    // gathered off the UI thread).
                    let foreground = pane
                        .pid
                        .and_then(|pid| foregrounds.get(&pid))
                        .cloned()
                        .unwrap_or_else(status::unknown_foreground_names);

                    // The pane the session badge/title follows: the focused
                    // pane, else the first pane of the screen.
                    let is_default_primary = Some(pane.id) == default_primary_id || pane_count == 1;
                    if focused || (is_default_primary && !primary_focused) {
                        primary_focused = focused;
                        primary_idx = Some(pane_idx);
                        primary_fg = Arc::clone(&foreground);
                    }

                    // Background panes never render, so their PTY events
                    // (title updates, child exit) must be drained here or the
                    // status would go stale while the user is in another tab.
                    if !focused {
                        pane.terminal.update(cx, |term, cx| {
                            term.drain_background_events(cx);
                        });
                    }

                    // Keep the pane header title in step with the OSC title
                    // even for background panes (the callback only fires where
                    // a window is attached).
                    let title_changed = {
                        let terminal = pane.terminal.read(cx);
                        let title = terminal.title().unwrap_or("");
                        let trimmed = title.trim();
                        if !pane.files_active
                            && pane.browser.is_none()
                            && pane.markdown_preview.is_none()
                            && pane.diff_preview.is_none()
                            && !trimmed.is_empty()
                            && pane.title.as_str() != trimmed
                        {
                            pane.title = trimmed.to_string();
                            if !pane.tool_panel {
                                pane.display_title =
                                    truncate_title(&SharedString::from(trimmed.to_string()));
                            }
                            true
                        } else {
                            false
                        }
                    };
                    if title_changed {
                        changed = true;
                    }

                    // Native chat owns its status while shown or running in the
                    // background; shell output and cached PTY evidence cannot
                    // overwrite an agent turn.
                    if pane.chat.active || pane.chat.running {
                        let next = if pane.chat.running && pane.chat.status != SessionStatus::Error
                        {
                            SessionStatus::Working
                        } else if pane.chat.status == SessionStatus::Idle && focused {
                            SessionStatus::Active
                        } else {
                            pane.chat.status
                        };
                        changed |= pane.status != next;
                        pane.status = next;
                        pane.is_agent = true;
                        pane.detection = status::DetectionResult::new(
                            next,
                            status::EvidenceSource::AgentLifecycle,
                            status::Confidence::High,
                        );
                        pane.poll_cache = None;
                        continue;
                    }

                    // Idle fast path: the screen revision changes on PTY output
                    // and resize, while focus, exit state, and foreground-group
                    // changes are checked independently. Stable panes therefore
                    // skip row extraction, string allocation, and screen hashing.
                    // The foreground snapshot is compared by pointer: the /proc
                    // cache returns the same allocation while the group is
                    // unchanged, so this is a load and a compare.
                    let (screen_revision, exited, mut agent_report, osc_progress) = {
                        let terminal = pane.terminal.read(cx);
                        (
                            terminal.screen_revision(),
                            terminal.has_exited(),
                            terminal.agent_status_report(),
                            terminal.osc_progress().to_owned(),
                        )
                    };
                    // Real activity (PTY output or a grid resize) restarts
                    // the hibernation clock. A hibernated pane is excluded:
                    // its screen is frozen by definition, and touching it
                    // here would make it impossible to fall asleep again.
                    let terminal_hibernated = pane.terminal.read(cx).is_hibernated();
                    let output_changed = pane
                        .poll_cache
                        .as_ref()
                        .is_none_or(|cached| cached.0 != screen_revision);
                    if !terminal_hibernated {
                        let now = std::time::Instant::now();
                        if pane.lifecycle.state().is_hibernated() {
                            // The view woke itself (a render or an input) before
                            // this tick: restart the clock instead of leaving a
                            // stale state that could never fall asleep again.
                            pane.lifecycle.woke(now);
                        } else if output_changed {
                            pane.lifecycle.touch(now);
                        }
                    }
                    let report_was_cleared = agent_report.is_none() && pane.osc_report_active;
                    pane.osc_report_active = agent_report.is_some();
                    if report_was_cleared {
                        pane.status = SessionStatus::Idle;
                        pane.detection = status::DetectionResult::new(
                            SessionStatus::Idle,
                            status::EvidenceSource::NoSignal,
                            status::Confidence::None,
                        );
                        pane.ack_screen = None;
                        pane.native_status = None;
                        pane.native_acknowledged = true;
                        pane.pending_approvals.clear();
                        pane.osc_report_active = false;
                    }
                    let foreground_changed =
                        pane.poll_cache
                            .as_ref()
                            .is_some_and(|(_, _, _, old_foreground)| {
                                !Arc::ptr_eq(old_foreground, &foreground)
                            });
                    let new_foreground_job =
                        pane.poll_cache.as_ref().is_some_and(|(_, _, _, old)| {
                            foreground_changed
                                && !foreground.is_empty()
                                && !Arc::ptr_eq(&foreground, &status::unknown_foreground_names())
                                && !Arc::ptr_eq(old, &status::unknown_foreground_names())
                        });
                    if foreground_changed && new_foreground_job && !foreground.is_empty() {
                        pane.native_acknowledged = false;
                        pane.ack_screen = None;
                        if matches!(pane.status, SessionStatus::Done | SessionStatus::Error) {
                            // A new command invalidates a completed job's badge.
                            pane.status = SessionStatus::Idle;
                            pane.detection = status::DetectionResult::new(
                                SessionStatus::Idle,
                                status::EvidenceSource::NoSignal,
                                status::Confidence::None,
                            );
                            pane.native_status = None;
                            pane.pending_approvals.clear();
                            if agent_report.as_ref().is_some_and(|report| {
                                matches!(
                                    report.state,
                                    terminal::AgentReportState::Done
                                        | terminal::AgentReportState::Error
                                        | terminal::AgentReportState::Idle
                                )
                            }) {
                                pane.terminal.update(cx, |terminal, _| {
                                    terminal.clear_agent_status_reports()
                                });
                                agent_report = None;
                            }
                            changed = true;
                        }
                    }
                    let unchanged = pane.poll_cache.as_ref().is_some_and(
                        |(revision, was_focused, was_exited, old_foreground)| {
                            *revision == screen_revision
                                && *was_focused == focused
                                && *was_exited == exited
                                && Arc::ptr_eq(old_foreground, &foreground)
                        },
                    );
                    // The grid only moved if the revision did; a focus or
                    // process-group change still needs a fresh verdict but can
                    // reuse the rows already in the scratch buffer.
                    let screen_unchanged = pane
                        .poll_cache
                        .as_ref()
                        .is_some_and(|(revision, ..)| *revision == screen_revision);
                    if unchanged {
                        match pane.status {
                            // A finished pane stays latched until new evidence.
                            SessionStatus::Done => continue,
                            // Working panes still need the detector's idle
                            // transition, but the grid did not move since the
                            // last scrape: reuse the cached rows instead of
                            // re-extracting and re-allocating the screen.
                            SessionStatus::Working | SessionStatus::Unknown => {}
                            _ if pane.detector.is_settling() => {}
                            _ => continue,
                        }
                    }

                    // Scrape into the pane's reused buffer only after the cheap
                    // revision/focus/process checks show that something changed.
                    // Ask for the whole visible screen: fresh shells can print
                    // their prompt on row 0, not just the last dozen rows.
                    if !screen_unchanged || pane.screen_scratch.is_empty() {
                        let diag_scrape = diagnostics::scrape_start();
                        pane.terminal
                            .read(cx)
                            .bottom_lines_into(usize::MAX, &mut pane.screen_scratch);
                        diagnostics::scrape_end(diag_scrape);
                    }

                    let screen_fp = {
                        let terminal = pane.terminal.read(cx);
                        screen_fingerprint(&pane.screen_scratch, terminal.title().unwrap_or(""))
                    };
                    let poll_key = (screen_revision, focused, exited, Arc::clone(&foreground));

                    let mut matched_agent_rule = false;
                    let mut matched_agent_idle = false;
                    let mut reported_agent_state = false;
                    let (next, ack) = {
                        let diag_detect = diagnostics::detect_start();
                        let terminal = pane.terminal.read(cx);
                        let bottom_lines = &pane.screen_scratch;
                        let title = terminal.title().unwrap_or("");
                        let screen_text = bottom_lines.join("\n");
                        let detector_state = pane.detector.update(
                            &screen_text,
                            terminal.has_exited(),
                            std::time::Instant::now(),
                        );
                        // while the screen is unchanged the sticky badge stays
                        // cleared; new output re-arms it.
                        let ack = pane.ack_screen;
                        let acknowledged = ack == Some(screen_fp) || pane.native_acknowledged;

                        // The agent rule match behind this tick's verdict, for the
                        // debug dump. The live path below captures it for free;
                        // the exited path only needs it for the dump itself.
                        let mut debug_match = None;

                        // Exit of the PTY child wins: the real exit code decides
                        // Done (0) vs Error (non-zero / signal), Shob-style.
                        let next = if terminal.has_exited() {
                            if debug {
                                debug_match = status::explain(&StatusInput {
                                    lines: bottom_lines,
                                    title: Some(title),
                                    foreground: &foreground,
                                    detector_state: Some(detector_state),
                                    osc_progress: &osc_progress,
                                });
                            }
                            if acknowledged {
                                SessionStatus::Idle
                            } else {
                                match terminal.exit_status() {
                                    Some(0) | None => SessionStatus::Done,
                                    Some(_) => SessionStatus::Error,
                                }
                            }
                        } else if let Some(native) = pane.native_status {
                            reported_agent_state = true;
                            native
                        } else if let Some(report) = &agent_report {
                            reported_agent_state = true;
                            match report.state {
                                terminal::AgentReportState::Idle => {
                                    if focused {
                                        SessionStatus::Active
                                    } else {
                                        SessionStatus::Idle
                                    }
                                }
                                terminal::AgentReportState::Working => SessionStatus::Working,
                                terminal::AgentReportState::Done => SessionStatus::Done,
                                terminal::AgentReportState::Blocked => SessionStatus::Blocked,
                                terminal::AgentReportState::Error => SessionStatus::Error,
                            }
                        } else {
                            let input = StatusInput {
                                lines: bottom_lines,
                                title: Some(title),
                                foreground: &foreground,
                                detector_state: Some(detector_state),
                                osc_progress: &osc_progress,
                            };
                            // The match is already computed by detection, so the
                            // debug dump can use it without a second evaluation.
                            let (detected, rule_match) = status::evaluate_screen_with_match(
                                &screen_text,
                                &input,
                                pane.status,
                                focused,
                            );
                            matched_agent_rule = rule_match.is_some();
                            matched_agent_idle = rule_match.as_ref().is_some_and(|matched| {
                                matched.state == agent_rules::RuleState::Idle
                                    && !matched.skip_state_update
                            });
                            if !debug {
                                debug_match = None;
                            } else {
                                debug_match = rule_match.clone();
                            }
                            if acknowledged
                                && matches!(detected, SessionStatus::Done | SessionStatus::Error)
                            {
                                if focused {
                                    SessionStatus::Active
                                } else {
                                    SessionStatus::Idle
                                }
                            } else {
                                detected
                            }
                        };

                        if debug {
                            let rule = debug_match.as_ref().map_or_else(
                                || "(no rule)".to_string(),
                                |m| {
                                    format!(
                                        "{}/{} v{} source={} state={} priority={} region={} \
                                         visible(working/blocker/idle)={}/{}/{} skip_update={}",
                                        m.pack,
                                        m.rule,
                                        m.pack_version.as_deref().unwrap_or("?"),
                                        m.source,
                                        m.state.label(),
                                        m.priority,
                                        m.region,
                                        m.visible_working,
                                        m.visible_blocker,
                                        m.visible_idle,
                                        m.skip_state_update,
                                    )
                                },
                            );
                            let evidence = if terminal.has_exited() {
                                "process-exit"
                            } else if reported_agent_state {
                                "agent-report"
                            } else if matched_agent_rule {
                                "agent-rule"
                            } else if matches!(
                                next,
                                SessionStatus::Blocked
                                    | SessionStatus::Error
                                    | SessionStatus::Working
                            ) {
                                "screen-heuristic"
                            } else if !foreground.is_empty() {
                                "foreground-process"
                            } else {
                                "no-signal"
                            };
                            let report_state = agent_report
                                .as_ref()
                                .map(|report| format!("{:?}", report.state));
                            eprintln!(
                                "[status] pane={} focused={focused} exited={} exit={:?} \
                                 pid={:?} fg={foreground:?} pack={:?} title={title:?} \
                                 prev={} next={} detector={detector_state:?} evidence={evidence} \
                                 report_state={report_state:?} rule={rule}",
                                pane.id,
                                terminal.has_exited(),
                                terminal.exit_status(),
                                pane.pid,
                                status::pack_for(&foreground),
                                pane.status.label(),
                                next.label(),
                            );
                            let screen: Vec<&str> = bottom_lines
                                .iter()
                                .map(String::as_str)
                                .filter(|line| !line.trim().is_empty())
                                .collect();
                            let from = screen.len().saturating_sub(DEBUG_SCREEN_LINES);
                            for line in &screen[from..] {
                                eprintln!("[status]   | {line}");
                            }
                        }

                        diagnostics::detect_end(diag_detect);
                        (next, ack)
                    };

                    // A done badge needs a recognized agent to have been working
                    // and then either exit or return to its explicit idle rule.
                    // Merely having a quiet foreground process is not completion.
                    let agent_now = foreground.iter().any(|name| status::is_agent_process(name));
                    let process_state_unavailable = foreground
                        .iter()
                        .any(|name| name == "<process-state-unavailable>");
                    let agent_exited =
                        pane.agent_active && !agent_now && !process_state_unavailable;
                    if !process_state_unavailable {
                        pane.agent_active = agent_now;
                    }

                    let turn_finished = !reported_agent_state
                        && pane.status == SessionStatus::Working
                        && (matched_agent_idle || agent_exited)
                        && matches!(next, SessionStatus::Idle | SessionStatus::Active)
                        && (agent_now || agent_exited);
                    let just_finished = turn_finished;
                    let next = if just_finished
                        && matches!(next, SessionStatus::Idle | SessionStatus::Active)
                        && ack != Some(screen_fp)
                    {
                        SessionStatus::Done
                    } else {
                        next
                    };
                    if matches!(next, SessionStatus::Working | SessionStatus::Blocked) {
                        pane.native_acknowledged = false;
                    }

                    // Keep the result visible across focus changes and trailing
                    // idle reports. A new working/blocked state, foreground job,
                    // or explicit clear starts a new lifecycle.
                    let next = latch_completion(pane.status, next);

                    pane.ack_screen = ack;
                    let evidence = if exited {
                        (
                            status::EvidenceSource::ProcessExit,
                            status::Confidence::High,
                        )
                    } else if reported_agent_state {
                        (
                            status::EvidenceSource::AgentReport,
                            status::Confidence::High,
                        )
                    } else if matched_agent_rule {
                        (status::EvidenceSource::AgentRule, status::Confidence::High)
                    } else if agent_exited {
                        (
                            status::EvidenceSource::AgentLifecycle,
                            status::Confidence::High,
                        )
                    } else if matches!(
                        next,
                        SessionStatus::Blocked | SessionStatus::Error | SessionStatus::Working
                    ) {
                        (
                            status::EvidenceSource::ScreenHeuristic,
                            status::Confidence::Low,
                        )
                    } else if next == SessionStatus::Unknown && !foreground.is_empty() {
                        (
                            status::EvidenceSource::ForegroundProcess,
                            status::Confidence::Low,
                        )
                    } else {
                        (status::EvidenceSource::NoSignal, status::Confidence::None)
                    };
                    let previous_status = pane.status;
                    pane.detection = status::DetectionResult::new(next, evidence.0, evidence.1);
                    if trace && previous_status != next {
                        eprintln!(
                            "[status-trace] pane={} {} -> {} evidence={:?} confidence={:?}",
                            pane.id,
                            previous_status.label(),
                            next.label(),
                            evidence.0,
                            evidence.1
                        );
                    }
                    if pane.status != next {
                        let prev_status = pane.status;
                        pane.status = next;
                        changed = true;

                        if prev_status != SessionStatus::Working && next == SessionStatus::Working {
                            pane.bottom_input_visible = false;
                            pane.completion_menu = None;
                            pane.terminal
                                .update(cx, |term, _| term.set_read_only(false));
                        } else if (prev_status == SessionStatus::Working
                            && next != SessionStatus::Working)
                            || (next == SessionStatus::Blocked
                                && prev_status != SessionStatus::Blocked)
                        {
                            pane.bottom_input_visible = true;
                        }
                    }

                    if let Some(app) = agent_report
                        .as_ref()
                        .and_then(|report| report.app.clone())
                        .filter(|app| !app.is_empty())
                    {
                        pane.app = Some(SharedString::from(app));
                    }
                    pane.is_agent |= reported_agent_state
                        || matched_agent_rule
                        || agent_now
                        || pane.app.is_some();

                    pane.poll_cache = Some(poll_key);
                }

                // Session badge: the highest-ranked pane status wins, so one
                // blocked pane inside a screen is enough to flag the tab.
                let mut aggregate = SessionStatus::Idle;
                for pane in &session.panes {
                    if pane.status.rank() > aggregate.rank() {
                        aggregate = pane.status;
                    }
                }
                if session.status != aggregate {
                    session.status = aggregate;
                    changed = true;
                }

                // Sidebar title/logo follow the pane the user is working in.
                if let Some(idx) = primary_idx {
                    let (
                        primary_title,
                        primary_status,
                        conversation_title,
                        reported_app,
                        first_provider,
                    ) = {
                        let pane = &session.panes[idx];
                        (
                            pane.title.clone(),
                            pane.status,
                            pane.chat.conversation_title().map(str::to_string),
                            (pane.osc_report_active || pane.native_status.is_some())
                                .then(|| pane.app.clone())
                                .flatten(),
                            pane.chat.first_response_provider.clone(),
                        )
                    };
                    let mut next_logo = logos::resolve_session_logo(
                        &primary_fg,
                        Some(&primary_title),
                        primary_status,
                        &session.logo,
                    );
                    if next_logo == logos::default_logo()
                        && let Some(path) = reported_app
                            .as_deref()
                            .and_then(|app| logos::registry().find_logo(app))
                    {
                        next_logo = SharedString::from(path.to_string());
                    }
                    if let Some(provider) = first_provider.as_deref() {
                        next_logo = provider_icon_path(provider);
                    }
                    if session.logo != next_logo {
                        session.logo = next_logo;
                        changed = true;
                    }
                    // An agent conversation is named by its own title; only
                    // panes without one fall back to the shell's OSC title.
                    let sidebar_title = conversation_title.unwrap_or(primary_title);
                    if session.title.as_str() != sidebar_title {
                        let shared = SharedString::from(sidebar_title);
                        session.title = shared;
                        session.display_title = truncate_title(&session.title);
                        changed = true;
                        title_changed = true;
                    }
                }
            }
        }
        // Shob's attention pass. It finalizes every pane's badge: a finish the
        // person missed becomes Done, and looking at a Done pane clears it. It
        // runs for every pane every tick because visibility and window focus
        // change without any screen output.
        let visible_ids = self.visible_pane_ids();
        let window_active = self.window_active;
        let focused_id = self.focused_pane_id;
        let primed = self.status_primed;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut attention_seq = self.attention_seq;
        let mut alerts: Vec<attention::AppAlert> = Vec::new();
        let mut attention_changed = false;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let focused = focused_id == Some(pane.id);
                    let looking = visible_ids.contains(&pane.id) && window_active;
                    let prev = pane.attention.last;
                    let before = pane.status;
                    let resolved = attention::resolve(
                        &mut pane.attention,
                        pane.status,
                        pane.is_agent
                            && pane.detection.evidence == status::EvidenceSource::AgentReport,
                        focused,
                        looking,
                        now_ms,
                        &mut attention_seq,
                    );
                    // Looking clears a finish for good: acknowledge it so a
                    // sticky native/OSC `done` cannot re-arm when you look away.
                    if looking
                        && before == SessionStatus::Done
                        && resolved.status != SessionStatus::Done
                    {
                        pane.native_status = None;
                        pane.chat.status = SessionStatus::Idle;
                        pane.native_acknowledged = true;
                        pane.ack_screen =
                            Some(screen_fingerprint(&pane.screen_scratch, &pane.title));
                        pane.osc_report_active = false;
                        pane.pending_approvals.clear();
                        pane.poll_cache = None;
                        pane.terminal
                            .update(cx, |terminal, _| terminal.clear_agent_status_reports());
                    }
                    if pane.status != resolved.status {
                        pane.status = resolved.status;
                        attention_changed = true;
                    }
                    if primed
                        && let Some(kind) = attention::alert_for(prev, resolved.status, looking)
                    {
                        alerts.push(attention::AppAlert {
                            kind,
                            pane_id: pane.id,
                            name: pane_agent_name(pane),
                            place: format!("{} · {}", project.name, session.display_title),
                            looking,
                        });
                    }
                }
                // Fold the finalized pane badges back into the session rollup.
                let mut aggregate = SessionStatus::Idle;
                for pane in &session.panes {
                    if pane.status.rank() > aggregate.rank() {
                        aggregate = pane.status;
                    }
                }
                if session.status != aggregate {
                    session.status = aggregate;
                    attention_changed = true;
                }
            }
        }
        self.attention_seq = attention_seq;
        self.status_primed = true;
        if !alerts.is_empty() {
            self.pending_alerts.extend(alerts);
        }
        changed |= attention_changed;
        #[cfg(unix)]
        if let Some(bus) = STATUS_BUS.as_ref()
            && status_snapshot_due()
        {
            let paths: Vec<_> = self
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .map(|pane| {
                    let (evidence, confidence) =
                        match (pane.detection.evidence, pane.detection.confidence) {
                            (status::EvidenceSource::ProcessExit, status::Confidence::High) => {
                                ("process-exit", "high")
                            }
                            (status::EvidenceSource::AgentReport, status::Confidence::High) => {
                                ("agent-report", "high")
                            }
                            (status::EvidenceSource::AgentRule, status::Confidence::High) => {
                                ("agent-rule", "high")
                            }
                            (status::EvidenceSource::AgentLifecycle, status::Confidence::High) => {
                                ("agent-lifecycle", "high")
                            }
                            (status::EvidenceSource::TerminalProgress, _) => {
                                ("terminal-progress", "medium")
                            }
                            (status::EvidenceSource::ScreenHeuristic, _) => {
                                ("screen-heuristic", "low")
                            }
                            (status::EvidenceSource::ForegroundProcess, _) => {
                                ("foreground-process", "low")
                            }
                            _ => ("no-signal", "none"),
                        };
                    let snapshot = status_bus::StatusSnapshot {
                        version: 1,
                        app_id: bus.app_id.clone(),
                        pane_id: pane.id as u64,
                        pane_generation: pane.generation.clone(),
                        state: pane.status.label().into(),
                        evidence: evidence.into(),
                        confidence: confidence.into(),
                        reporter: pane.native_session_reporter.clone(),
                        updated_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    };
                    (
                        status_bus::snapshot_path(bus, pane.id, &pane.generation),
                        snapshot,
                    )
                })
                .collect();
            cx.background_executor()
                .spawn(async move {
                    for (path, snapshot) in paths {
                        let Ok(bytes) = serde_json::to_vec(&snapshot) else {
                            continue;
                        };
                        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
                        if std::fs::write(&temp, bytes).is_ok() {
                            let _ = std::fs::rename(temp, path);
                        }
                    }
                })
                .detach();
        }
        let session_changed = self.release_inactive_resources(cx);
        diagnostics::panes_polled(panes_polled);
        diagnostics::poll_end(diag_poll);
        if title_changed || session_changed {
            self.schedule_workspace_save(cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// Make a project the one shown in the workspace and focus its current
    /// pane. Expanding it is part of activation: selecting a project should
    /// always reveal the sessions that belong to it.
    fn activate_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };
        self.active_project_idx = project_idx;
        project.expanded = true;
        let session_idx = project
            .active_session_idx
            .min(project.sessions.len().saturating_sub(1));
        project.active_session_idx = session_idx;
        let target_pane_id = project
            .sessions
            .get(session_idx)
            .and_then(|s| s.panes.first().map(|pane| pane.id));
        if let Some(pane_id) = target_pane_id {
            self.focus_workspace_pane(pane_id, window, cx);
            if let Some(pane) = self.pane_ref(pane_id) {
                let dir = self.agent_working_directory(pane);
                self.schedule_git_refresh(dir, cx);
            }
        } else {
            self.focused_pane_id = None;
            cx.notify();
        }
        self.schedule_workspace_save(cx);
    }

    #[cfg(test)]
    fn toggle_project_expanded(&mut self, project_idx: usize, cx: &mut Context<Self>) {
        if let Some(project) = self.projects.get_mut(project_idx) {
            project.expanded = !project.expanded;
            cx.notify();
            self.schedule_workspace_save(cx);
        }
    }

    /// Create a session inside one specific project, switching the workspace
    /// to that project so the new terminal is the one being worked in. This
    /// backs the "+" on every project header, not just the active project.
    fn create_new_session_in_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_project_session(project_idx, self.settings.agent_layout, window, cx);
    }
}
