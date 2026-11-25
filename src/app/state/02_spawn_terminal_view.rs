impl AppState {
    fn spawn_terminal_view(
        dir: Option<&PathBuf>,
        palette: ColorPalette,
        window_title_target: gpui::WeakEntity<Self>,
        pane_id: usize,
        scrollback: Option<String>,
        scrollback_lines: usize,
        cx: &mut Context<Self>,
    ) -> Result<(
        Entity<TerminalView>,
        Option<u32>,
        String,
        gpui::Subscription,
    )> {
        #[cfg(unix)]
        let generation = status_bus::new_generation();
        #[cfg(not(unix))]
        let generation = format!("pane-{pane_id}-{}", std::process::id());
        #[cfg(unix)]
        if let Some(bus) = STATUS_BUS.as_ref() {
            shob_compat::register(&bus.app_id, pane_id, &generation);
        }
        let startup_generation = generation.clone();
        let dir = dir.cloned();
        let pending = pty_session::PendingPty::deferred(move || {
            let generation = startup_generation;
            let (shell_cmd, shell_name) = detect_shell();
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| anyhow::anyhow!("PTY open error: {e}"))?;
            let mut cmd = CommandBuilder::new(&shell_cmd);
            if launched_by_cargo() {
                cmd.env_remove("CARGO_MANIFEST_DIR");
            }
            if let Some(d) = &dir {
                cmd.cwd(d);
            }
            shell_integration::configure_shell(&mut cmd, &shell_name);
            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");
            cmd.env("TERM_PROGRAM", "shob");
            cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
            cmd.env("LANG", "en_US.UTF-8");
            cmd.env("LC_ALL", "en_US.UTF-8");
            if let Ok(executable) = std::env::current_exe() {
                cmd.env("SHOB_EXECUTABLE", &executable);
                cmd.env("MARKUP_EXECUTABLE", &executable);
            }
            #[cfg(unix)]
            if let Some(bus) = STATUS_BUS.as_ref() {
                cmd.env("SHOB_STATUS_SOCKET", &bus.socket);
                cmd.env("SHOB_APP_ID", &bus.app_id);
                cmd.env("SHOB_PANE_ID", pane_id.to_string());
                cmd.env("SHOB_PANE_GENERATION", &generation);
                cmd.env("MARKUP_STATUS_SOCKET", &bus.socket);
                cmd.env("MARKUP_APP_ID", &bus.app_id);
                cmd.env("MARKUP_PANE_ID", pane_id.to_string());
                cmd.env("MARKUP_PANE_GENERATION", &generation);
                // Scoped vendor reporters bind to this exact pane generation.
                cmd.env("SHOB_ENV", "1");
                cmd.env("SHOB_SOCKET_PATH", &bus.socket);
                cmd.env("SHOB_PANE_ID", &generation);
                cmd.env("HERDR_ENV", "1");
                cmd.env("HERDR_SOCKET_PATH", &bus.socket);
                cmd.env("HERDR_PANE_ID", &generation);
                cmd.env_remove("CMD_SHOB_PANE_OWNER");
                cmd.env_remove("CMD_HERDR_PANE_OWNER");
                if let Ok(executable) = std::env::current_exe() {
                    cmd.env("SHOB_BIN_PATH", &executable);
                    cmd.env("HERDR_BIN_PATH", executable);
                }
                let snapshot_path = status_bus::snapshot_path(bus, pane_id, &generation);
                cmd.env("SHOB_STATUS_SNAPSHOT", &snapshot_path);
                cmd.env("MARKUP_STATUS_SNAPSHOT", &snapshot_path);
            }
            #[cfg(unix)]
            if let Some(tty) = pair.master.tty_name() {
                cmd.env("SHOB_STATUS_TTY", &tty);
                cmd.env("MARKUP_STATUS_TTY", &tty);
            }
            pty_session::PreparedPty::spawn(pair, cmd)
        });
        let pty_session::PendingPty {
            reader,
            writer,
            control,
            events,
        } = pending;

        let config = TerminalConfig {
            font_family: MONO_FONT.into(),
            font_size: px(13.5),
            cols: 80,
            rows: 24,
            scrollback: scrollback_lines.max(1),
            line_height_multiplier: 1.2,
            padding: Edges::all(px(8.0)),
            colors: palette,
            shell_token: shell_integration::shell_token().to_string(),
            cursor_blink: true,
            copy_on_select: true,
            right_click_paste: false,
            alternate_scroll: true,
            detect_path_links: true,
            show_scrollbar: true,
            read_only: true,
            ..TerminalConfig::default()
        };
        let start_control = control.clone();

        let resize_callback = move |cols: usize, rows: usize| {
            control.resize(PtySize {
                cols: cols as u16,
                rows: rows as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        };

        let target = window_title_target.clone();
        let link_target = window_title_target.clone();
        let terminal = cx.new(|cx| {
            let mut view =
                TerminalView::new_with_scrollback(writer, reader, config, scrollback, cx)
                    .with_resize_callback(resize_callback)
                    .with_start_callback(move || start_control.start())
                    .with_link_callback(move |window, cx, link| {
                        let (path, _, _) = link.path_with_position();
                        let mut handled = false;
                        let _ = link_target.update(cx, |state, cx| {
                            handled = state.open_local_link(pane_id, &path, window, cx);
                        });
                        handled
                    })
                    .with_title_callback(move |window, cx, title| {
                        window.set_window_title(title);
                        let title_str = title.to_string();
                        let _ = target.update(cx, |state, cx| {
                            state.update_pane_title(pane_id, &title_str, cx);
                        });
                    });
            view.set_capture_output(false);
            view
        });

        // Setup, resize, child waiting and teardown happen away from GPUI.
        // This relay owns only weak UI handles: closing a pane drops control.
        let target = window_title_target;
        let terminal_weak = terminal.downgrade();
        let event_generation = generation.clone();
        cx.spawn(async move |_, cx| {
            while let Ok(event) = events.recv().await {
                match event {
                    pty_session::PtyEvent::Ready { pid } => {
                        let _ = target.update(cx, |state, cx| {
                            if let Some(pane) = state.pane_mut(pane_id)
                                && pane.generation == event_generation
                            {
                                pane.pid = pid;
                                cx.notify();
                            }
                        });
                    }
                    pty_session::PtyEvent::Exited(status) => {
                        let _ = terminal_weak.update(cx, |term, cx| {
                            term.record_child_exit(status.exit_code() as i32, cx);
                        });
                    }
                    pty_session::PtyEvent::Error(error) => {
                        #[cfg(unix)]
                        shob_compat::unregister(&event_generation);
                        let _ = terminal_weak.update(cx, |term, cx| {
                            term.report_io_error(error, cx);
                        });
                    }
                }
            }
        })
        .detach();

        // Shell-integration events arrive on the UI thread from the reader task.
        // The subscription is stored on the pane so it survives until the pane
        // is closed.
        let terminal_subscription = cx.subscribe(
            &terminal,
            move |state, _terminal, event: &ShellEvent, cx| {
                state.on_shell_event(pane_id, event, cx);
            },
        );

        Ok((terminal, None, generation, terminal_subscription))
    }

    fn update_pane_title(&mut self, pane_id: usize, title: &str, cx: &mut Context<Self>) {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        if pane.files_active
            || pane.browser.is_some()
            || pane.markdown_preview.is_some()
            || pane.diff_preview.is_some()
            || pane.title == trimmed
        {
            return;
        }
        pane.title = trimmed.to_string();
        let next_display = truncate_title(&SharedString::from(trimmed.to_string()));
        if !pane.tool_panel {
            pane.display_title = next_display.clone();
        }
        // Mirror the pane the user is working in onto the session's sidebar
        // entry. Either the focused pane or the first pane of the screen
        // (when focus has not moved yet) is the primary.
        let is_primary = self.focused_pane_id == Some(pane_id)
            || session
                .pane_tree
                .as_ref()
                .is_some_and(|t| t.first_leaf() == pane_id);
        if is_primary {
            session.title = SharedString::from(trimmed.to_string());
            session.display_title = next_display.clone();
            let title_logo =
                logos::resolve_session_logo(&[], Some(trimmed), session.status, &session.logo);
            if title_logo != logos::default_logo() && session.logo != title_logo {
                session.logo = title_logo;
            }
        }
        self.mark_dirty(cx);
        cx.notify();
    }

    /// Periodic detection tick, Shob-style: screen scrape + OSC title +
    /// foreground process tree, with sticky Done/Error and focus resolution.
    /// Every pane of every session is polled; the session badge is the
    /// highest-ranked pane status and the sidebar title/logo follow the pane
    /// the user is working in.
    ///
    /// This variant gathers process information on the caller's thread. The app
    /// loop uses [`Self::poll_sessions_status_with`] so the blocking `/proc`
    /// reads run on the background executor; this entry point remains for tests
    /// and for callers that already have a snapshot.
    #[allow(dead_code)]
    pub fn poll_sessions_status(&mut self, cx: &mut Context<Self>) {
        let foregrounds = self.gather_foreground_processes();
        self.poll_sessions_status_with(&foregrounds, cx);
    }

    /// Shell pid of every live pane, deduplicated. Cheap and UI-thread safe:
    /// the background inspector needs a list, not an entity borrow.
    pub fn pane_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        for project in &self.projects {
            for session in &project.sessions {
                for pane in &session.panes {
                    if let Some(pid) = pane.pid
                        && !pids.contains(&pid)
                    {
                        pids.push(pid);
                    }
                }
            }
        }
        pids
    }

    #[cfg(unix)]
    fn apply_status_events(
        &mut self,
        events: Vec<status_bus::StatusEvent>,
        cx: &mut Context<Self>,
    ) {
        let Some(bus) = STATUS_BUS.as_ref() else {
            return;
        };
        let mut routed = Vec::new();
        for event in events {
            if event.app_id != bus.app_id {
                continue;
            }
            let Some(source) = self
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .find(|pane| {
                    pane.id as u64 == event.pane_id && pane.generation == event.pane_generation
                })
            else {
                continue;
            };
            if let Some(session_id) = event.session_id.as_ref() {
                for target in self
                    .projects
                    .iter()
                    .flat_map(|project| &project.sessions)
                    .flat_map(|session| &session.panes)
                    .filter(|pane| {
                        pane.id != source.id
                            && pane.native_session_id.as_ref() == Some(session_id)
                            && pane.native_session_reporter.as_deref()
                                == Some(event.reporter.as_str())
                    })
                {
                    let mut fanout = event.clone();
                    fanout.pane_id = target.id as u64;
                    fanout.pane_generation = target.generation.clone();
                    fanout.reporter_generation = format!("session-route:{session_id}");
                    fanout.sequence = FANOUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                    routed.push((fanout, true));
                }
            }
            routed.push((event, false));
        }
        for (event, is_routed) in routed {
            if event.app_id != bus.app_id {
                continue;
            }
            let Some(&(project_idx, session_idx, pane_idx)) =
                self.pane_index.get(&(event.pane_id as usize))
            else {
                continue;
            };
            let Some(pane) = self
                .projects
                .get_mut(project_idx)
                .and_then(|p| p.sessions.get_mut(session_idx))
                .and_then(|s| s.panes.get_mut(pane_idx))
            else {
                continue;
            };
            if pane.generation != event.pane_generation {
                continue;
            }
            if event.agent_id.is_some() {
                // Workers report to the parent pane; they never own its session.
            } else if event.event == "register" {
                if let Some(session_id) = event.session_id.as_ref() {
                    if pane
                        .native_session_id
                        .as_ref()
                        .is_some_and(|old| old != session_id)
                    {
                        pane.native_status = None;
                        pane.native_acknowledged = false;
                        pane.pending_approvals.clear();
                        pane.native_turn_id = None;
                        pane.ack_screen = None;
                        pane.poll_cache = None;
                        pane.status = SessionStatus::Unknown;
                    }
                    pane.native_session_id = Some(session_id.clone());
                    pane.native_session_reporter = Some(event.reporter.clone());
                }
            } else if let Some(session_id) = event.session_id.as_ref() {
                match (
                    pane.native_session_reporter.as_deref(),
                    pane.native_session_id.as_ref(),
                ) {
                    (Some(reporter), _) if reporter != event.reporter => continue,
                    (_, Some(current)) if current != session_id => continue,
                    _ => {
                        pane.native_session_id = Some(session_id.clone());
                        pane.native_session_reporter = Some(event.reporter.clone());
                    }
                }
            }

            let source = if let Some(agent_id) = event.agent_id.as_ref() {
                format!("worker:{}:{agent_id}", event.reporter)
            } else if is_routed {
                format!(
                    "route:{}:{}",
                    event.reporter,
                    event.session_id.as_deref().unwrap_or("")
                )
            } else {
                format!("parent:{}:{}", event.reporter, event.reporter_generation)
            };
            if !is_routed {
                let reporter_key = if event.agent_id.is_some() {
                    source.clone()
                } else {
                    format!("parent:{}", event.reporter)
                };
                let active_generation = pane.native_reporter_generation.get(&reporter_key);
                if active_generation.is_some_and(|active| active != &event.reporter_generation) {
                    if event.event != "register" {
                        continue;
                    }
                    if event.agent_id.is_some() {
                        pane.native_sequence.remove(&source);
                    } else {
                        let prefix = format!("parent:{}:", event.reporter);
                        pane.native_sequence
                            .retain(|key, _| !key.starts_with(&prefix));
                    }
                }
                pane.native_reporter_generation
                    .insert(reporter_key, event.reporter_generation.clone());
            }
            if pane
                .native_sequence
                .get(&source)
                .is_some_and(|previous| event.sequence <= *previous)
            {
                continue;
            }
            pane.native_sequence.insert(source, event.sequence);
            // A worker reports to its parent pane: update the indented entry and
            // never let it drive the pane's own status or an alert.
            if let Some(agent_id) = event.agent_id.clone() {
                let status = match event.event.as_str() {
                    "working" => Some(SessionStatus::Working),
                    "blocked" => Some(SessionStatus::Blocked),
                    "done" => Some(SessionStatus::Done),
                    "error" => Some(SessionStatus::Error),
                    "idle" | "register" => Some(SessionStatus::Idle),
                    "clear" | "cancelled" => None,
                    _ => continue,
                };
                match status {
                    Some(status) => {
                        let since = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let entry = pane.sub_agents.entry(agent_id.clone()).or_insert(
                            attention::SubAgent {
                                id: agent_id.clone(),
                                title: event.message.clone(),
                                status: SessionStatus::Idle,
                                since,
                                seq: since,
                            },
                        );
                        entry.status = status;
                        entry.since = since;
                        entry.seq = since;
                        if entry.title.is_none() {
                            entry.title = event.message.clone();
                        }
                    }
                    None => {
                        pane.sub_agents.remove(&agent_id);
                    }
                }
                continue;
            }
            let reporter_name = event.reporter.clone();
            let event_name = event.event.clone();
            if pane.native_acknowledged && matches!(event.event.as_str(), "done" | "error") {
                continue;
            }
            if let Some(turn_id) = event.turn_id.as_ref() {
                if pane
                    .native_turn_id
                    .as_ref()
                    .is_some_and(|old| old != turn_id)
                    && matches!(event.event.as_str(), "working" | "blocked")
                {
                    pane.pending_approvals.clear();
                    pane.native_status = None;
                    pane.ack_screen = None;
                }
                pane.native_turn_id = Some(turn_id.clone());
            }
            let next = match event.event.as_str() {
                "register" | "heartbeat" => None,
                "idle" => Some(latch_completion(
                    pane.native_status.unwrap_or(pane.status),
                    SessionStatus::Idle,
                )),
                "working" => {
                    pane.native_acknowledged = false;
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Working)
                }
                "blocked" => {
                    pane.native_acknowledged = false;
                    pane.pending_approvals
                        .insert(event.approval_id.unwrap_or_else(|| "__single__".into()));
                    Some(SessionStatus::Blocked)
                }
                "approval_resolved" => {
                    if let Some(id) = event.approval_id {
                        if !pane.pending_approvals.remove(&id) && pane.pending_approvals.len() == 1
                        {
                            pane.pending_approvals.clear();
                        }
                    } else if pane.pending_approvals.len() == 1 {
                        pane.pending_approvals.clear();
                    }
                    Some(if pane.pending_approvals.is_empty() {
                        SessionStatus::Working
                    } else {
                        SessionStatus::Blocked
                    })
                }
                "done" => {
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Done)
                }
                "error" => {
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Error)
                }
                "clear" | "cancelled" => {
                    pane.pending_approvals.clear();
                    pane.native_status = None;
                    pane.native_acknowledged = true;
                    pane.osc_report_active = false;
                    pane.terminal
                        .update(cx, |terminal, _| terminal.clear_agent_status_reports());
                    pane.status = SessionStatus::Unknown;
                    pane.ack_screen = None;
                    pane.poll_cache = None;
                    None
                }
                _ => None,
            };
            if let Some(next) = next {
                pane.native_status = Some(next);
                pane.status = next;
                pane.is_agent = true;
                if pane.app.is_none() && !reporter_name.is_empty() {
                    pane.app = Some(SharedString::from(reporter_name.clone()));
                }
                pane.detection = status::DetectionResult::new(
                    next,
                    status::EvidenceSource::AgentReport,
                    status::Confidence::High,
                );
                if matches!(next, SessionStatus::Working | SessionStatus::Blocked) {
                    pane.ack_screen = None;
                }
                pane.poll_cache = None;
            }
            self.integration_feedback =
                SharedString::from(format!("Live reporting from {reporter_name}: {event_name}"));
        }
        cx.notify();
    }

    /// Read `/proc` for every pane on the calling thread. The app loop uses
    /// [`Self::poll_sessions_status_with`] and gathers these on the background
    /// executor instead, so the blocking reads never sit in a frame.
    #[allow(dead_code)]
    fn gather_foreground_processes(&self) -> HashMap<u32, Arc<[String]>> {
        self.pane_pids()
            .into_iter()
            .map(|pid| (pid, status::foreground_process_names(Some(pid))))
            .collect()
    }
}
