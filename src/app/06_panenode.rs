impl PaneNode {
    fn has_terminal_layout_pane(&self, session: &Session) -> bool {
        match self {
            Self::Leaf(id) => session
                .panes
                .iter()
                .any(|pane| pane.id == *id && !pane.tool_panel && !pane.one_mode),
            Self::Split { left, right, .. } => {
                left.has_terminal_layout_pane(session) || right.has_terminal_layout_pane(session)
            }
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        match self {
            PaneNode::Leaf(leaf_id) => *leaf_id == id,
            PaneNode::Split { left, right, .. } => left.contains(id) || right.contains(id),
        }
    }

    pub fn split(
        &mut self,
        target_id: usize,
        new_id: usize,
        split_id: usize,
        axis: gpui::Axis,
    ) -> bool {
        match self {
            PaneNode::Leaf(id) => {
                if *id == target_id {
                    *self = PaneNode::Split {
                        id: split_id,
                        axis,
                        left: Box::new(PaneNode::Leaf(target_id)),
                        right: Box::new(PaneNode::Leaf(new_id)),
                    };
                    true
                } else {
                    false
                }
            }
            PaneNode::Split { left, right, .. } => {
                left.split(target_id, new_id, split_id, axis)
                    || right.split(target_id, new_id, split_id, axis)
            }
        }
    }

    fn preview_goes_left(&self, source: usize) -> Option<bool> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split {
                axis, left, right, ..
            } => {
                let (branch, on_right) = if left.contains(source) {
                    (left, false)
                } else if right.contains(source) {
                    (right, true)
                } else {
                    return None;
                };
                branch
                    .preview_goes_left(source)
                    .or_else(|| (*axis == gpui::Axis::Horizontal).then_some(on_right))
            }
        }
    }

    fn swap_split_children(&mut self, source: usize, preview: usize) {
        if let PaneNode::Split { left, right, .. } = self {
            if matches!((&**left, &**right), (PaneNode::Leaf(a), PaneNode::Leaf(b)) if *a == source && *b == preview)
            {
                std::mem::swap(left, right);
            } else {
                left.swap_split_children(source, preview);
                right.swap_split_children(source, preview);
            }
        }
    }

    pub fn remove(self, target_id: usize) -> Option<PaneNode> {
        match self {
            PaneNode::Leaf(id) => {
                if id == target_id {
                    None
                } else {
                    Some(PaneNode::Leaf(id))
                }
            }
            PaneNode::Split {
                id,
                axis,
                left,
                right,
            } => {
                let left = left.remove(target_id);
                let right = right.remove(target_id);
                match (left, right) {
                    (Some(l), Some(r)) => Some(PaneNode::Split {
                        id,
                        axis,
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    (Some(l), None) => Some(l),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                }
            }
        }
    }

    pub fn first_leaf(&self) -> usize {
        match self {
            PaneNode::Leaf(id) => *id,
            PaneNode::Split { left, .. } => left.first_leaf(),
        }
    }
}

fn editor_icon(name: &str) -> &'static str {
    match name {
        "Zed" => "icons/editors/zed.svg",
        "VS Code" => "icons/editors/vscode.svg",
        "VS Code Insiders" => "icons/editors/vscode-insiders.svg",
        "VSCodium" => "icons/editors/vscodium.svg",
        "Cursor" => "icons/editors/cursor.svg",
        "Trae" => "icons/editors/trae.svg",
        "Kiro" => "icons/editors/kiro.svg",
        "IntelliJ IDEA" => "icons/editors/intellijidea.svg",
        "Aqua" => "icons/editors/aqua.svg",
        "CLion" => "icons/editors/clion.svg",
        "DataGrip" => "icons/editors/datagrip.svg",
        "DataSpell" => "icons/editors/dataspell.svg",
        "GoLand" => "icons/editors/goland.svg",
        "PhpStorm" => "icons/editors/phpstorm.svg",
        "PyCharm" => "icons/editors/pycharm.svg",
        "Rider" => "icons/editors/rider.svg",
        "RubyMine" => "icons/editors/rubymine.svg",
        "RustRover" => "icons/editors/rustrover.svg",
        "WebStorm" => "icons/editors/webstorm.svg",
        _ => "icons/folder.svg",
    }
}

fn project_editor_icon(name: Option<&str>, color: Hsla) -> AnyElement {
    if name == Some("Antigravity") {
        img("icons/editors/antigravity.png")
            .size(px(14.0))
            .flex_none()
            .into_any_element()
    } else {
        svg()
            .path(name.map_or("icons/folder.svg", editor_icon))
            .size(px(14.0))
            .flex_none()
            .text_color(color)
            .into_any_element()
    }
}

pub struct Project {
    pub id: usize,
    /// Shared (cheap to clone) because the render pass copies it every frame.
    pub name: SharedString,
    pub path: PathBuf,
    pub sessions: Vec<Session>,
    pub active_session_idx: usize,
    /// Whether this project's session list is expanded in the sidebar. Every
    /// project keeps its own state, so several open projects can be browsed
    /// independently without switching away from the one being worked in.
    pub expanded: bool,
    /// Number of newest session titles revealed in the sidebar; reset on launch.
    visible_session_count: usize,
}

/// Storage order is oldest-first so restore, selection, and close indices stay
/// stable. The sidebar shows the newest `visible_count` sessions, newest first.
#[cfg(test)]
fn sidebar_session_indices(
    session_count: usize,
    visible_count: usize,
) -> impl Iterator<Item = usize> {
    let shown = visible_count.min(session_count);
    let oldest_shown = session_count - shown;
    (oldest_shown..session_count).rev()
}

/// Keep the active session reachable even when it is older than the preview.
#[cfg(test)]
fn sidebar_visible_sessions(count: usize, visible: usize, active: Option<usize>) -> Vec<usize> {
    let mut indices: Vec<_> = sidebar_session_indices(count, visible).collect();
    if let Some(active) = active.filter(|&index| index < count && !indices.contains(&index)) {
        indices.push(active);
    }
    indices
}

struct SidebarSearch {
    input: Entity<InputState>,
    previous_focus: Option<gpui::FocusHandle>,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarProjectPickerMode {
    Scope,
    NewSession,
}

struct SidebarProjectPicker {
    mode: SidebarProjectPickerMode,
    selected: usize,
    input: Entity<InputState>,
    previous_focus: Option<gpui::FocusHandle>,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, Debug)]
struct SidebarThreadMenuState {
    project_idx: usize,
    session_idx: usize,
    section: SidebarSection,
    pinned: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SidebarThreadState {
    pinned: bool,
    /// Lower values appear first among pinned threads; swaps persist reordering.
    pin_order: Option<u64>,
    settled: bool,
    snoozed_until_ms: Option<u64>,
    archived: bool,
    parked_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarSection {
    Pinned,
    Active,
    Snoozed,
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarShelf {
    Snoozed,
    Settled,
}

impl SidebarThreadState {
    fn section(self, now_ms: u64) -> Option<SidebarSection> {
        if self.archived {
            return None;
        }
        if self.snoozed_until_ms.is_some_and(|until| until > now_ms) {
            return Some(SidebarSection::Snoozed);
        }
        if self.settled {
            return Some(SidebarSection::Settled);
        }
        if self.pinned {
            return Some(SidebarSection::Pinned);
        }
        Some(SidebarSection::Active)
    }
}

#[derive(Clone, Copy, Debug)]
struct SidebarThreadRef {
    project_idx: usize,
    session_idx: usize,
    section: SidebarSection,
    pinned: bool,
    /// The active route is revealed in the inbox even when its metadata would
    /// otherwise put it in a closed shelf or out of the current project scope.
    revealed_active: bool,
}

#[derive(Default)]
struct SidebarThreadGroups {
    working: Vec<SidebarThreadRef>,
    working_count: usize,
    pinned: Vec<SidebarThreadRef>,
    active: Vec<SidebarThreadRef>,
    snoozed: Vec<SidebarThreadRef>,
    settled: Vec<SidebarThreadRef>,
    hidden_active: usize,
    hidden_settled: usize,
}

fn sidebar_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn sidebar_relative_time(now_ms: u64, since_ms: u64) -> String {
    if since_ms == 0 || since_ms > now_ms {
        return "now".to_string();
    }
    let seconds = (now_ms - since_ms) / 1000;
    if seconds < 60 {
        "now".to_string()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn sidebar_matches(query: &str, text: &str) -> bool {
    query.is_empty() || text.to_lowercase().contains(query)
}

const CHAT_SCROLLBAR_THUMB_WIDTH: Pixels = px(7.0);
const CHAT_SCROLLBAR_THUMB_INSET: Pixels = px(2.0);

#[derive(Clone)]
struct ChatScrollbarHandle {
    list: ListState,
    focused_track: Option<(gpui::EntityId, Rc<std::cell::Cell<Pixels>>)>,
}

impl ChatScrollbarHandle {
    fn element(&self) -> gpui_component::scroll::Scrollbar {
        gpui_component::scroll::Scrollbar::vertical(self)
            .thumb_geometry(CHAT_SCROLLBAR_THUMB_WIDTH, CHAT_SCROLLBAR_THUMB_INSET)
            .unthrottled_drag()
    }
}

impl gpui_component::scroll::ScrollbarHandle for ChatScrollbarHandle {
    fn offset(&self) -> Point<Pixels> {
        self.list.scroll_px_offset_for_scrollbar()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.list.set_offset_from_scrollbar(offset);
    }

    fn drag_to(&self, _: gpui::Axis, previous: Pixels, target: Pixels, range: Pixels) {
        if target == range {
            self.list.set_follow_mode(FollowMode::Tail);
            return;
        }
        let max = self.list.max_offset_for_scrollbar().y;
        let current = (-self.offset().y).clamp(px(0.), max);
        // The thumb's range is frozen, but measuring history changes the pixel map under it.
        let offset = if target < previous {
            current * (target / previous)
        } else {
            current + (max - current) * ((target - previous) / (range - previous))
        };
        self.set_offset(gpui::point(px(0.), -offset));
    }

    fn on_scroll(&self, cx: &mut App) {
        if let Some((transcript, _)) = &self.focused_track {
            cx.notify(*transcript);
        }
    }

    fn content_size(&self) -> gpui::Size<Pixels> {
        let viewport = self.list.viewport_bounds().size;
        let height = self
            .focused_track
            .as_ref()
            .map_or(viewport.height, |(_, height)| height.get());
        size(viewport.width, height + self.list.max_offset_for_scrollbar().y)
    }
}

struct FocusedChatScrollbar {
    pane_id: usize,
    transcript_view: gpui::EntityId,
    list: ListState,
    height: Rc<std::cell::Cell<Pixels>>,
}

impl Render for FocusedChatScrollbar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let height = self.height.clone();
        let scrollbar = ChatScrollbarHandle {
            list: self.list.clone(),
            focused_track: Some((self.transcript_view, height.clone())),
        }
        .element();
        div()
            .relative()
            .size_full()
            .child(
                canvas(
                    move |bounds, _, _| height.set(bounds.size.height),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .id(("focused-chat-scrollbar-track", self.pane_id))
                    .debug_selector(|| "focused-chat-scrollbar-track".into())
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(16.0))
                    .child(
                        scrollbar.id(("agent-chat-scrollbar", self.pane_id))
                            .scrollbar_show(gpui_component::scroll::ScrollbarShow::Always),
                    ),
            )
    }
}

struct AppState {
    projects: Vec<Project>,
    active_project_idx: usize,
    next_id: usize,
    palette: ColorPalette,
    /// O(1) session lookup by id: session id -> (project idx, session idx).
    session_index: HashMap<usize, (usize, usize)>,
    navigation_history: Vec<usize>,
    navigation_position: usize,
    /// O(1) pane lookup by id: pane id -> (project idx, session idx, pane idx).
    pane_index: HashMap<usize, (usize, usize, usize)>,
    /// Background-computed working-tree counts, shared by panes in a project.
    project_change_counts: HashMap<PathBuf, Option<project_diff::ChangeSummary>>,
    /// Cached Git workspace snapshots keyed by active directory.
    git_snapshots: HashMap<PathBuf, git_workspace::GitWorkspaceSnapshot>,
    /// Directories currently being inspected off-thread.
    git_refreshing: HashSet<PathBuf>,
    /// Agent panes currently creating a new worktree off-thread.
    git_worktree_creating: HashSet<usize>,
    /// Pane that currently owns keyboard focus, tracked every render frame
    /// and consumed by the status poller (Active state, sticky-state clearing).
    focused_pane_id: Option<usize>,
    pub active_view: ActiveView,
    pub theme_preference: ThemePreference,
    /// Desktop and terminal color palette.
    pub palette_scheme: PaletteScheme,
    default_agent_model: Option<agent_auth::AgentModel>,
    pub sidebar_visible: bool,
    compact_sidebar_open: bool,
    sidebar_search: Option<SidebarSearch>,
    sidebar_project_picker: Option<SidebarProjectPicker>,
    sidebar_thread_menu: Option<SidebarThreadMenuState>,
    system_snapshot: system_detection::Snapshot,
    system_monitor_open: bool,
    /// Sidebar-only thread state, keyed by the durable session uid.
    sidebar_threads: HashMap<String, SidebarThreadState>,
    settled_visible_count: usize,
    integration_panel_visible: bool,
    integration_feedback: SharedString,
    pub sidebar_width: Pixels,
    /// Shared gap between the sidebar's right edge and the pointer at the
    /// moment a resize drag started. Keeping the offset (instead of snapping
    /// the edge to the cursor) stops the sidebar jumping on a grab.
    sidebar_resize_offset: Option<Pixels>,
    /// True while the sidebar's right edge is being dragged, so the width is
    /// only persisted once the drag ends instead of on every mouse move.
    sidebar_resizing: bool,
    persistence_path: Option<PathBuf>,
    save_generation: Arc<AtomicU64>,
    startup_loaded: bool,
    quit_subscription: Option<gpui::Subscription>,
    /// Whether the OS considers this window focused, mirrored from the render
    /// pass. "Looking" requires it: a visible-but-backgrounded window hasn't
    /// been seen.
    window_active: bool,
    /// The first poll tick only primes the attention model; it never alerts.
    status_primed: bool,
    /// Alerts to drain on the next render, where a window is available to
    /// push notifications and play sounds.
    pending_alerts: Vec<attention::AppAlert>,
    /// Monotonic counter for "most recent status change" ordering.
    attention_seq: u64,
    /// Currently open project dropdown menu: Some(project_id).
    project_menu: Option<usize>,
    editor_picker_open: Option<usize>,
    project_script_form: Option<(usize, Entity<InputState>, Entity<InputState>)>,
    available_editors: Vec<(String, PathBuf)>,
    renaming_project: Option<RenamingProject>,
    pub terminal_context_menu: Option<TerminalContextMenu>,
    /// Ctrl+R reverse history search palette, if open.
    history_search: Option<HistorySearchState>,
    /// Lightbox for an attached image, if open.
    image_viewer: Option<ImageViewerState>,
    /// User-editable settings (also mirrored to `settings.json`).
    settings: settings::AppSettings,
    /// Set whenever durable state changes; the 30 s autosave drains it.
    dirty: bool,
    /// Last known window geometry, captured on close and while saving.
    last_window_bounds: Option<settings::SavedWindow>,
    /// False when launched with `SHOB_DISABLE_RESTORE`; the app then never
    /// writes to the saved state (read-only troubleshooting mode).
    persistence_enabled: bool,
    settings_section: SettingsSection,
    settings_focus: gpui::FocusHandle,
    update_state: updater::State,
    /// Two-step confirmation state for "clear saved sessions".
    clear_data_confirm: bool,
}

/// Full-window preview of one attached image.
struct ImageViewerState {
    path: PathBuf,
    name: String,
    dimensions: Option<(u32, u32)>,
}

/// Shob-style "search command history" palette.
struct HistorySearchState {
    pane_id: usize,
    input: Entity<InputState>,
    matches: Vec<String>,
    selected: usize,
    _subscription: gpui::Subscription,
}

struct BranchPickerState {
    input: Entity<InputState>,
    selected: usize,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalContextMenu {
    pub pane_id: usize,
    pub position: Point<Pixels>,
}

struct RenamingProject {
    project_id: usize,
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}
/// Data read off the UI thread before restoring a workspace.
struct RestoreData {
    workspace: workspace_state::SavedWorkspace,
    scrollbacks: HashMap<String, String>,
    transcripts: HashMap<String, conversations::RestoredConversation>,
    usage: HashMap<String, embedded::AgentUsage>,
}

fn transcript_key(uid: &str, key: &str) -> String {
    format!("{uid}|{key}")
}

fn save_pane_tree(node: &PaneNode) -> workspace_state::SavedPaneNode {
    match node {
        PaneNode::Leaf(pane) => workspace_state::SavedPaneNode::Leaf { pane: *pane },
        PaneNode::Split {
            id,
            axis,
            left,
            right,
        } => workspace_state::SavedPaneNode::Split {
            id: *id,
            axis: match axis {
                gpui::Axis::Horizontal => workspace_state::SavedAxis::Horizontal,
                gpui::Axis::Vertical => workspace_state::SavedAxis::Vertical,
            },
            left: Box::new(save_pane_tree(left)),
            right: Box::new(save_pane_tree(right)),
        },
    }
}

fn restore_pane_tree(node: &workspace_state::SavedPaneNode) -> PaneNode {
    match node {
        workspace_state::SavedPaneNode::Leaf { pane } => PaneNode::Leaf(*pane),
        workspace_state::SavedPaneNode::Split {
            id,
            axis,
            left,
            right,
        } => PaneNode::Split {
            id: *id,
            axis: match axis {
                workspace_state::SavedAxis::Horizontal => gpui::Axis::Horizontal,
                workspace_state::SavedAxis::Vertical => gpui::Axis::Vertical,
            },
            left: Box::new(restore_pane_tree(left)),
            right: Box::new(restore_pane_tree(right)),
        },
    }
}

/// True when every leaf refers to a pane that actually spawned.
fn tree_matches_panes(node: &PaneNode, panes: &[TerminalPane]) -> bool {
    match node {
        PaneNode::Leaf(id) => panes.iter().any(|pane| pane.id == *id),
        PaneNode::Split { left, right, .. } => {
            tree_matches_panes(left, panes) && tree_matches_panes(right, panes)
        }
    }
}

include!("state/01_saved_workspace.rs");
include!("state/02_spawn_terminal_view.rs");
include!("state/03_poll_sessions_status_with.rs");
include!("state/04_create_project_session.rs");
include!("state/05_finish_renaming_project.rs");
include!("state/06_render_preview_tab.rs");
include!("state/07_on_pane_input_changed.rs");
include!("state/08_update_chat_mode.rs");
include!("state/09_poll_agent_events.rs");
include!("state/10_start_agent_turn.rs");
include!("state/11_open_terminal_context_menu.rs");
include!("state/12_render_quick_action_item.rs");
include!("state/13_render_shob_input_bar.rs");
include!("state/14_render_agent_auth_layer.rs");
include!("state/15_render_agent_permission.rs");
include!("state/16_render_agent_auth.rs");
include!("state/17_render_chat_message.rs");
include!("state/18_render_agent_chat_body.rs");
include!("state/19_discover_project_editors.rs");
include!("state/20_render_terminal_pane.rs");
include!("state/21_render_t3_sidebar_actions.rs");
include!("state/22_render_sidebar_thread_card.rs");
include!("state/settings_navigation.rs");
include!("state/23_render_settings_page.rs");
include!("state/24_toggle_restore_option.rs");

/// Small square icon button used by the sidebar rows and the sidebar
/// toolbar. `on_click` is a `cx.listener`; the button stops mouse propagation
/// so clicking it never also activates the row it sits in.
fn sidebar_icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    icon_size: f32,
    tokens: &ThemeTokens,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let hover_bg = tokens.sidebar_icon_btn_hover;
    let icon_color = tokens.sidebar_icon;
    let icon_hover_color = tokens.sidebar_icon_hover;
    div()
        .id(id)
        .size(px(20.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor(CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
        .child(
            svg()
                .path(icon)
                .size(px(icon_size))
                .text_color(icon_color)
                .hover(move |s| s.text_color(icon_hover_color)),
        )
}

/// Compact square button for T3 Code's single-row sidebar header and its
/// icon-only footer. It leaves click handling to the caller so shift-click can
/// be read from `ClickEvent::modifiers()`.
fn t3_sidebar_icon_action(
    _state: &AppState,
    id: &'static str,
    icon: &'static str,
    title: &'static str,
    active: bool,
    tokens: ThemeTokens,
) -> gpui::Stateful<gpui::Div> {
    let hover_bg = tokens.sidebar_icon_btn_hover;
    let icon_color = tokens.sidebar_icon;
    let icon_hover_color = tokens.sidebar_icon_hover;
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .rounded(px(7.0))
        .when(active, move |button| button.bg(tokens.sidebar_item_active))
        .hover(move |button| button.bg(hover_bg))
        .cursor(CursorStyle::PointingHand)
        .child(
            svg()
                .path(icon)
                .size(px(15.0))
                .text_color(icon_color)
                .hover(move |s| s.text_color(icon_hover_color)),
        )
        .tooltip(move |_, cx| {
            cx.new(|_| gpui_component::tooltip::Tooltip::new(SharedString::from(title)))
                .into()
        })
}

fn sidebar_text_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    icon: &'static str,
    primary: bool,
    tokens: &ThemeTokens,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let bg_color = if primary {
        tokens.primary_btn_bg
    } else {
        tokens.secondary_btn_bg
    };
    let border_color = if primary {
        tokens.primary_btn_bg
    } else {
        tokens.border
    };
    let hover_color = if primary {
        tokens.primary_btn_hover
    } else {
        tokens.secondary_btn_hover
    };
    let text_color = if primary {
        tokens.primary_btn_fg
    } else {
        tokens.secondary_btn_fg
    };
    div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .rounded(px(6.0))
        .cursor(CursorStyle::PointingHand)
        .flex()
        .items_center()
        .gap(px(6.0))
        .bg(bg_color)
        .border_1()
        .border_color(border_color)
        .hover(move |s| s.bg(hover_color))
        .child(svg().path(icon).size(px(13.0)).text_color(text_color))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(text_color)
                .child(label),
        )
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}

#[cfg(test)]
thread_local! {
    static APP_ROOT_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
