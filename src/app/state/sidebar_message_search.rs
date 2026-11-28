const SIDEBAR_MESSAGE_SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(120);
const SIDEBAR_MESSAGE_SEARCH_LIMIT: usize = 80;

struct SidebarSearch {
    input: Entity<InputState>,
    previous_focus: Option<gpui::FocusHandle>,
    selected: Option<usize>, // Session ID, not a position that can move as hits arrive.
    scroll: gpui::ScrollHandle,
    query: String,
    messages: SidebarMessageSearchState,
    generation: Arc<AtomicU64>,
    worker_running: bool,
    _subscription: gpui::Subscription,
}

impl SidebarSearch {
    fn new(input: Entity<InputState>, subscription: gpui::Subscription) -> Self {
        Self {
            input,
            previous_focus: None,
            selected: None,
            scroll: gpui::ScrollHandle::new(),
            query: String::new(),
            messages: SidebarMessageSearchState::Idle,
            generation: Arc::new(AtomicU64::new(0)),
            worker_running: false,
            _subscription: subscription,
        }
    }
}

impl Drop for SidebarSearch {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

enum SidebarMessageSearchState {
    Idle,
    Pending,
    Ready { hits: Vec<SidebarMessageHit>, failed: bool },
}

struct SidebarMessageHit {
    session_id: usize,
    role: ChatRole,
    snippet: SharedString,
}

struct SidebarSearchRow<'a> {
    thread: SidebarThreadRef,
    message: Option<&'a SidebarMessageHit>,
}

impl AppState {
    fn update_sidebar_message_search(&mut self, cx: &mut Context<Self>) {
        let query = self.sidebar_search_query(cx);
        let Some(search) = self.sidebar_search.as_mut() else {
            return;
        };
        if search.query == query {
            return;
        }
        search.selected = None;
        search.scroll.set_offset(gpui::Point::default());
        search.generation.fetch_add(1, Ordering::Relaxed);
        let pending = !query.is_empty() && self.settings.restore.agent_history;
        search.query = query;
        search.messages = if pending {
            SidebarMessageSearchState::Pending
        } else {
            SidebarMessageSearchState::Idle
        };
        cx.notify();
        if !pending || search.worker_running {
            return;
        }

        // A single pump owns the worker. Edits cancel the current scan and
        // replace its queued query instead of starting concurrent scans.
        search.worker_running = true;
        let generation = Arc::clone(&search.generation);
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            loop {
                let Ok(Some(request)) = this.update(cx, |this, _| {
                    let search = this.sidebar_search.as_mut()?;
                    if matches!(search.messages, SidebarMessageSearchState::Pending) {
                        Some(generation.load(Ordering::Relaxed))
                    } else {
                        search.worker_running = false;
                        None
                    }
                }) else {
                    break;
                };
                executor.timer(SIDEBAR_MESSAGE_SEARCH_DELAY).await;
                if generation.load(Ordering::Relaxed) != request {
                    continue;
                }
                let Ok(Some((query, targets))) = this.update(cx, |this, _| {
                    let query = &this.sidebar_search.as_ref()?.query;
                    Some((query.clone(), this.sidebar_message_search_targets(query)))
                }) else {
                    break;
                };
                let worker_generation = Arc::clone(&generation);
                let result = executor.spawn(async move {
                    search_sidebar_messages(&query, targets, || {
                        worker_generation.load(Ordering::Relaxed) != request
                    })
                }).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(search) = this.sidebar_search.as_mut()
                        && search.generation.load(Ordering::Relaxed) == request
                    {
                        search.messages = result;
                        cx.notify();
                    }
                });
            }
        }).detach();
    }

    fn sidebar_message_search_targets(&self, query: &str) -> Vec<(usize, Vec<String>)> {
        let title_ids: HashSet<_> = self.sidebar_search_thread_refs(query).into_iter()
            .map(|row| self.sidebar_thread_id(row))
            .collect();
        self.projects.iter().flat_map(|project| &project.sessions)
            .filter(|session| {
                !session.panes.is_empty()
                    && !self.sidebar_thread_state(&session.uid).archived
                    && !title_ids.contains(&session.id)
            })
            .map(|session| (session.id, session.panes.iter().map(|pane| pane.uid.clone()).collect()))
            .collect()
    }

    fn sidebar_message_search_state(&self, query: &str) -> Option<&SidebarMessageSearchState> {
        self.sidebar_search.as_ref()
            .filter(|search| self.settings.restore.agent_history && search.query == query)
            .map(|search| &search.messages)
    }

    fn sidebar_search_rows(&self, query: &str) -> Vec<SidebarSearchRow<'_>> {
        let mut rows: Vec<_> = self.sidebar_search_thread_refs(query).into_iter()
            .map(|thread| SidebarSearchRow { thread, message: None })
            .collect();
        let Some(SidebarMessageSearchState::Ready { hits, .. }) = self.sidebar_message_search_state(query) else {
            return rows;
        };
        if hits.is_empty() {
            return rows;
        }
        let title_ids: HashSet<_> = rows.iter().map(|row| self.sidebar_thread_id(row.thread)).collect();
        let now = sidebar_now_ms();
        let active = self.sidebar_active_thread_key();
        rows.extend(hits.iter().filter_map(|hit| {
            if title_ids.contains(&hit.session_id) {
                return None;
            }
            let &(project_idx, session_idx) = self.session_index.get(&hit.session_id)?;
            let session = self.projects.get(project_idx)?.sessions.get(session_idx)?;
            if session.id != hit.session_id {
                return None;
            }
            let state = self.sidebar_thread_state(&session.uid);
            Some(SidebarSearchRow {
                thread: SidebarThreadRef {
                    project_idx,
                    session_idx,
                    section: state.section(now)?,
                    pinned: state.pinned,
                    revealed_active: active == Some((project_idx, session_idx)),
                },
                message: Some(hit),
            })
        }));
        rows
    }

    fn render_sidebar_search_results(
        &self,
        query: &str,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let rows = self.sidebar_search_rows(query);
        let active = self.sidebar_active_thread_key();
        let now = sidebar_now_ms();
        let mut list = div().id("sidebar-search-results")
            .role(gpui::Role::Group)
            .aria_label("Thread search results")
            .w_full().flex_1().min_h_0().flex().flex_col().gap(px(1.0)).overflow_y_scroll();
        let search = self.sidebar_search.as_ref();
        if let Some(search) = search {
            list = list.track_scroll(&search.scroll);
        }
        if rows.is_empty() {
            let copy = match self.sidebar_message_search_state(query) {
                Some(SidebarMessageSearchState::Pending) => "Searching thread messages…",
                Some(SidebarMessageSearchState::Ready { failed: true, .. }) => "Couldn't search thread messages",
                _ => "No threads found",
            };
            return list.child(div()
                .id("sidebar-search-empty")
                .debug_selector(|| "sidebar-search-empty".into())
                .role(gpui::Role::Status)
                .py(px(24.0)).px(px(8.0)).text_center().text_xs().text_color(tokens.sidebar_muted)
                .child(copy));
        }
        let selected = search.and_then(|search| search.selected);
        for SidebarSearchRow { thread, message } in rows {
            let session_id = self.sidebar_thread_id(thread);
            let row = match message {
                Some(hit) => self.render_sidebar_message_result(thread, hit, now, tokens, cx),
                None => self.render_sidebar_thread_slim(thread, active, now, tokens, cx),
            };
            list = list.child(row
                .when(selected == Some(session_id), |row| row.bg(tokens.sidebar_item_active).border_color(tokens.ring))
                .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                    if let Some(search) = this.sidebar_search.as_mut()
                        && search.selected != Some(session_id)
                    {
                        search.selected = Some(session_id);
                        cx.notify();
                    }
                })));
        }
        list
    }

    fn render_sidebar_message_result(
        &self,
        thread: SidebarThreadRef,
        hit: &SidebarMessageHit,
        now: u64,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let project = &self.projects[thread.project_idx];
        let session = &project.sessions[thread.session_idx];
        let row_id = format!("session-row-{}", session.id);
        let (status, color) = self.sidebar_thread_status(session, now, tokens);
        let bot = self.bots.get(&session.uid);
        let (label, color_hint) = if hit.role == ChatRole::User {
            ("You: ", tokens.pane_border_focused)
        } else {
            ("Agent: ", tokens.block_success)
        };
        let row = div()
            .id(ElementId::Name(row_id.clone().into()))
            .debug_selector(move || row_id.clone())
            .group("sidebar-search-message")
            .h(SIDEBAR_THREAD_SLIM_HEIGHT + px(16.0))
            .w_full().flex_none().flex().items_center().gap(px(10.0)).px(px(9.0)).rounded(px(6.0))
            .cursor_pointer()
            .when(thread.revealed_active, |row| row.bg(tokens.sidebar_item_active).text_color(tokens.sidebar_item_active_fg))
            .when(!thread.revealed_active, |row| row.hover(move |style| style.bg(tokens.sidebar_item_hover)))
            .when_some(bot, |row, bot| {
                row.child(sidebar_bot_icon(bot_avatar::avatar(bot.avatar, SIDEBAR_BOT_ART_SIZE)))
            })
            .when(bot.is_none(), |row| {
                row.child(project_favicon(project, tokens, SIDEBAR_THREAD_ICON_SIZE))
            })
            .child(div().min_w_0().flex_1().flex().flex_col()
                .child(self.render_sidebar_thread_title(session))
                .child(div().id("sidebar-search-snippet").debug_selector(|| "sidebar-search-snippet".into())
                    .min_w_0().flex().text_xs().text_color(tokens.sidebar_muted)
                    .child(div().flex_none().mr(px(4.0)).text_color(color_hint).child(label))
                    .child(div().min_w_0().truncate().child(hit.snippet.clone()))))
            .when(bot.is_none(), |row| {
                row.child(session_logo_icon(session.logo.clone(), tokens.sidebar_icon))
            })
            .child(self.sidebar_thread_status_menu(thread, status, color, "sidebar-search-message", tokens, cx));
        self.sidebar_row_interactions(row, thread, session, tokens, cx)
            .aria_description(format!("{}{}. {}{}",
                if thread.revealed_active { "Current thread. " } else { "" },
                project.name, label, hit.snippet))
    }
}

fn search_sidebar_messages(
    query: &str,
    mut targets: Vec<(usize, Vec<String>)>,
    is_stale: impl Fn() -> bool,
) -> SidebarMessageSearchState {
    let mut hits = Vec::new();
    let mut failed = false;
    targets.sort_unstable_by_key(|(id, _)| std::cmp::Reverse(*id));
    let root = paths::conversations_dir();
    let mut search = conversations::MessageSearch::new(query);
    for (session_id, panes) in targets {
        if is_stale() || hits.len() == SIDEBAR_MESSAGE_SEARCH_LIMIT {
            break;
        }
        let (hit, failed_pane) = search.search_panes(&root, &panes, &is_stale);
        failed |= failed_pane;
        if let Some((role, snippet)) = hit {
            hits.push(SidebarMessageHit { session_id, role, snippet: snippet.into() });
        }
    }
    SidebarMessageSearchState::Ready { hits, failed }
}
