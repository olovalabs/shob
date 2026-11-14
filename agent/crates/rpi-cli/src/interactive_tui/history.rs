//! Bounded resident history, independent of the prompt mailbox.
use super::*;
use rpi_harness::session::types::{
    BranchEntryPage, BranchEntryPageQuery, BranchOverview, SessionStorage,
};
use rpi_harness::session::Session;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Older,
    Newer,
    Latest,
}
impl Direction {
    pub(super) fn parse(args: &str) -> Option<Self> {
        match args.trim() {
            "" | "older" => Some(Self::Older),
            "newer" => Some(Self::Newer),
            "latest" => Some(Self::Latest),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Resident {
    generation: u64,
    storage: Option<Arc<dyn SessionStorage>>,
    // Entries belong to rendered components, not a second retained payload copy.
    page: Option<BranchEntryPage>,
    error: Option<Arc<Container>>,
    // Page-local runtime notices retire with that page, including late callbacks.
    notices: Arc<Container>,
}
#[derive(Default)]
pub(super) struct HistoryUi {
    resident: Mutex<Resident>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) unsaved: std::sync::atomic::AtomicBool,
    pub(super) active: Arc<Container>,
    pub(super) panels: Arc<Container>,
    pub(super) header: Arc<Container>,
    /// Startup notices must stay before the live transcript slot. If they are
    /// appended to `chat` directly, streamed output (which belongs to `active`)
    /// renders above the notice box.
    pub(super) updates: Arc<Container>,
}
impl HistoryUi {
    pub(super) fn invalidate(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.abort();
        }
        self.resident.lock().unwrap().generation += 1;
    }
    pub(super) fn reset(&self) {
        self.invalidate();
        let mut resident = self.resident.lock().unwrap();
        resident.generation += 1;
        resident.page = None;
        resident.error = None;
        resident.notices = Arc::new(Container::new());
        resident.storage = None;
        self.unsaved
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.active.clear();
        self.panels.clear();
        self.header.clear();
        self.updates.clear();
    }
    pub(super) fn bind(&self, harness: &AgentHarness) {
        self.invalidate();
        let mut resident = self.resident.lock().unwrap();
        resident.generation += 1;
        let storage = harness.session().storage();
        if !resident
            .storage
            .as_ref()
            .is_some_and(|bound| Arc::ptr_eq(bound, &storage))
        {
            resident.page = None;
            resident.notices = Arc::new(Container::new());
        }
        resident.storage = Some(storage);
    }
    pub(super) fn is_bound(&self) -> bool {
        self.resident.lock().unwrap().storage.is_some()
    }
    pub(super) fn notice_container(&self, root: &Arc<Container>) -> Arc<Container> {
        let resident = self.resident.lock().unwrap();
        if resident.storage.is_none() {
            root.clone()
        } else if resident
            .page
            .as_ref()
            .is_some_and(|page| page.has_more_after)
        {
            resident.notices.clone()
        } else {
            self.active.clone()
        }
    }
    pub(super) fn is_latest(&self) -> bool {
        self.resident
            .lock()
            .unwrap()
            .page
            .as_ref()
            .is_none_or(|page| !page.has_more_after)
    }
    fn mount_header(&self, chat: &Arc<Container>) {
        let header: Arc<dyn Component> = self.header.clone();
        if self.header.child_count() > 0
            && !chat
                .get_children()
                .iter()
                .any(|child| Arc::ptr_eq(child, &header))
        {
            chat.add_child(header);
        }
    }
    // Reserve this slot even while empty: the background update check may
    // complete after assistant streaming has already begun.
    pub(super) fn mount_updates(&self, chat: &Arc<Container>) {
        let updates: Arc<dyn Component> = self.updates.clone();
        if !chat
            .get_children()
            .iter()
            .any(|child| Arc::ptr_eq(child, &updates))
        {
            chat.add_child(updates);
        }
    }
    pub(super) fn mount_active(&self, chat: &Arc<Container>) {
        self.mount_header(chat);
        self.mount_updates(chat);
        let active: Arc<dyn Component> = self.active.clone();
        if !chat
            .get_children()
            .iter()
            .any(|child| Arc::ptr_eq(child, &active))
        {
            chat.add_child(active);
        }
        let panels: Arc<dyn Component> = self.panels.clone();
        if !chat
            .get_children()
            .iter()
            .any(|child| Arc::ptr_eq(child, &panels))
        {
            chat.add_child(panels);
        }
    }
}

pub(super) fn apply_overview(footer: &FooterComponent, overview: &BranchOverview) {
    let mut totals = rpi_tui::footer::UsageTotals::default();
    totals.add(
        overview.input,
        overview.output,
        overview.cache_read,
        overview.cache_write,
        overview.cost_total,
    );
    footer.set_usage(totals);
    footer.set_cache_hit_rate(overview.cache_hit_rate);
    footer.set_context_tokens(overview.context_tokens);
}

/// Initial/session-switch replay. Storage is pinned before any asynchronous read.
pub(super) async fn restore(harness: &AgentHarness, state: &Arc<TuiState>, chat: &Arc<Container>) {
    state.history_page.bind(harness);
    restore_bound(state, chat).await;
}

async fn restore_bound(state: &Arc<TuiState>, chat: &Arc<Container>) {
    // Even an empty or unreadable session must have a visible live-operation slot.
    state.history_page.mount_header(chat);
    state.history_page.mount_active(chat);
    let (generation, storage) = {
        let resident = state.history_page.resident.lock().unwrap();
        (resident.generation, resident.storage.clone().unwrap())
    };
    let tree = Session::new(storage.clone(), None).view("main");
    let Ok(expected_leaf) = tree.get_leaf_id().await else {
        return;
    };
    let Ok(page) = tree.branch_entry_page(&Default::default()).await else {
        return;
    };
    let panels = Arc::new(Container::new());
    restore_history_panels(&tree, &panels).await;
    let overview = tree.branch_overview().await.ok();
    let name = tree.get_name().await.ok().flatten();
    if page.leaf_id != expected_leaf || tree.get_leaf_id().await.ok() != Some(expected_leaf) {
        return;
    }
    let mut resident = state.history_page.resident.lock().unwrap();
    if !current(&resident, generation, &storage) {
        return;
    }
    let page_chat = render_page(state, &page.entries);
    resident.notices = Arc::new(Container::new());
    page_chat.add_child(resident.notices.clone());
    clear_settled_active(state);
    chat.clear();
    state.history_page.mount_header(chat);
    state.history_page.mount_updates(chat);
    chat.add_child(page_chat);
    resident.error = None;
    state.history_page.panels.clear();
    for child in panels.get_children() {
        state.history_page.panels.add_child(child);
    }
    state.history_page.mount_active(chat);
    state.footer.set_session_name(name.as_deref());
    if let Some(overview) = overview {
        apply_overview(&state.footer, &overview);
    }
    resident.page = Some(metadata(page));
    state.search.activate();
    state.search.deactivate();
    state.search_bar.set_visible(false);
    if let Some(tui) = &state.tui {
        if let Some(scroll) = tui.get_primary_scroll_view() {
            scroll.invalidate();
        }
        tui.clear_selection();
        tui.request_render(false);
    }
}

fn clear_settled_active(state: &TuiState) {
    // Keep a new prompt reservation out until the previous active slot is retired.
    let status = state.status.lock().unwrap();
    if *status == RunStatus::Idle && !state.user_bash_running() {
        state.history_page.active.clear();
        state.current_assistant.lock().unwrap().take();
        state.tool_components.lock().unwrap().clear();
        state.bash_components.lock().unwrap().clear();
    }
}

fn metadata(mut page: BranchEntryPage) -> BranchEntryPage {
    page.entries.clear();
    page
}
fn current(resident: &Resident, generation: u64, storage: &Arc<dyn SessionStorage>) -> bool {
    resident.generation == generation
        && resident
            .storage
            .as_ref()
            .is_some_and(|bound| Arc::ptr_eq(bound, storage))
}
fn render_page(state: &TuiState, entries: &[Entry]) -> Arc<Container> {
    let chat = Arc::new(Container::new());
    render_history_entries(
        entries.to_vec(),
        &chat,
        state.markdown_transformer(),
        Some(state.extension_session.clone()),
        state.images_visible(),
    );
    state.apply_history_tool_expansion(&chat);
    chat
}

/// A task per request, never sent through TuiMessage: that mailbox awaits a
/// complete provider run. Last request wins; session/operation changes advance
/// the same generation before mutating their backing state.
pub(super) fn request(state: Arc<TuiState>, tui: Arc<TuiAltScreen>, direction: Direction) {
    // A failed write may leave the screen as the only copy: preserve it.
    if state
        .history_page
        .unsaved
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let mut worker = state.history_page.worker.lock().unwrap();
    if let Some(previous) = worker.take() {
        previous.abort();
    }
    let (generation, storage, previous) = {
        let mut resident = state.history_page.resident.lock().unwrap();
        let Some(storage) = resident.storage.clone() else {
            return;
        };
        resident.generation += 1;
        (resident.generation, storage, resident.page.clone())
    };
    let task_state = state.clone();
    *worker = Some(tokio::spawn(async move {
        let state = task_state;
        let tree = Session::new(storage.clone(), None).view("main");
        let Ok(expected_leaf) = tree.get_leaf_id().await else {
            return;
        };
        let result = async {
            let previous = match previous {
                Some(page) => page,
                None => tree.branch_entry_page(&Default::default()).await?,
            };
            let query = match direction {
                Direction::Latest => BranchEntryPageQuery::default(),
                Direction::Older if previous.has_more_before => BranchEntryPageQuery {
                    leaf_id: previous.leaf_id,
                    before_id: previous.next_before_id,
                    ..Default::default()
                },
                Direction::Newer if previous.has_more_after => BranchEntryPageQuery {
                    leaf_id: previous.leaf_id,
                    after_id: previous.next_after_id,
                    ..Default::default()
                },
                _ => return Ok(None),
            };
            tree.branch_entry_page(&query).await.map(Some)
        }
        .await;
        // A branch move or appended operation invalidates the captured snapshot.
        if tree.get_leaf_id().await.ok() != Some(expected_leaf) {
            return;
        }
        let mut resident = state.history_page.resident.lock().unwrap();
        if !current(&resident, generation, &storage)
            || state
                .history_page
                .unsaved
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match result {
            Ok(Some(page)) => {
                let page_chat = render_page(&state, &page.entries);
                resident.notices = Arc::new(Container::new());
                page_chat.add_child(resident.notices.clone());
                clear_settled_active(&state);
                resident.error = None;
                // Drop every reference to the old page (including image components).
                state.chat_container.clear();
                state.history_page.mount_header(&state.chat_container);
                state.history_page.mount_updates(&state.chat_container);
                state.chat_container.add_child(page_chat);
                if !page.has_more_after {
                    state
                        .chat_container
                        .add_child(state.history_page.active.clone());
                }
                state
                    .chat_container
                    .add_child(state.history_page.panels.clone());
                if let Some(scroll) = tui.get_primary_scroll_view() {
                    scroll.invalidate();
                }
                tui.clear_selection();
                state.search.activate(); // clears query/matches referencing old rows
                state.search.deactivate();
                state.search_bar.set_visible(false);
                resident.page = Some(metadata(page));
                match direction {
                    Direction::Older => tui.scroll_to_bottom(),
                    Direction::Newer => tui.scroll_to_top(),
                    Direction::Latest => tui.scroll_to_bottom(),
                }
                tui.request_render(false); // invalidate scroll/render/image upload caches
            }
            Ok(None) => {}
            Err(error) => {
                // Failure keeps the original page and component references intact.
                if let Some(previous) = resident.error.take() {
                    let previous: Arc<dyn Component> = previous;
                    state.chat_container.remove_child(&previous);
                }
                let notice = Arc::new(Container::new());
                add_error_message(&notice, &format!("Could not load history: {error}"));
                state.chat_container.add_child(notice.clone());
                resident.error = Some(notice);
                tui.request_render(false);
            }
        }
    }));
}

/// Only operation settlement reloads latest, never individual token events.
pub(super) fn settle(state: &Arc<TuiState>, tui: &Arc<TuiAltScreen>) {
    // A failed write may leave the screen as the only copy: preserve it.
    if state
        .history_page
        .unsaved
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    if state.history_page.is_latest() {
        request(state.clone(), tui.clone(), Direction::Latest);
    } else {
        state.history_page.invalidate();
        clear_settled_active(state);
    }
}

pub(super) fn handle_key(
    state: &Arc<TuiState>,
    tui: &Arc<TuiAltScreen>,
    bindings: &rpi_tui::Keybindings,
    key: &KeyEvent,
) -> bool {
    // Never steal editor navigation or selector input.
    if !state.editor.get_text().is_empty()
        || state.selector_open()
        || state.search.is_active()
        || state.extension_dialog_open()
    {
        return false;
    }
    let Some(scroll) = tui.get_primary_scroll_view() else {
        return false;
    };
    let direction = if bindings.matches(key, rpi_tui::keybindings::keys::BOTTOM)
        && !state.history_page.is_latest()
    {
        Some(Direction::Latest)
    } else if bindings.matches(key, rpi_tui::keybindings::keys::PAGE_UP) && scroll.scroll_top() == 0
    {
        state
            .history_page
            .resident
            .lock()
            .unwrap()
            .page
            .as_ref()
            .filter(|page| page.has_more_before)
            .map(|_| Direction::Older)
    } else if bindings.matches(key, rpi_tui::keybindings::keys::PAGE_DOWN)
        && scroll.scroll_top().saturating_add(scroll.viewport_height()) >= scroll.content_height()
    {
        state
            .history_page
            .resident
            .lock()
            .unwrap()
            .page
            .as_ref()
            .filter(|page| page.has_more_after)
            .map(|_| Direction::Newer)
    } else {
        None
    };
    if let Some(direction) = direction {
        request(state.clone(), tui.clone(), direction);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_arguments_are_explicit_and_default_to_older() {
        for (arg, expected) in [
            ("", Direction::Older),
            (" older ", Direction::Older),
            ("newer", Direction::Newer),
            ("latest", Direction::Latest),
        ] {
            assert_eq!(Direction::parse(arg), Some(expected));
        }
        for invalid in ["all", "older newer", "latest extra"] {
            assert_eq!(Direction::parse(invalid), None);
        }
    }
}

#[cfg(test)]
#[path = "history/tests.rs"]
mod paging_tests;
