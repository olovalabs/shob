//! Durable workspace state: project/terminal-tab/split-tree layout, per-pane
//! working directories, and captured terminal scrollback.
//!
//! `workspace.json` is auto-managed and debounced; scrollback is stored in
//! separate per-pane files so the hot save path stays small. Legacy v1 files
//! are backed up to `<data>/backups/` before being converted.

use crate::agent_auth::AgentModel;
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub const CURRENT_VERSION: u32 = 2;
const MAX_SCROLLBACK_BYTES: usize = 256 * 1024;
const MAX_SCROLLBACK_LINES: usize = 2_000;
const MAX_COMMAND_HISTORY: usize = 200;

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedAxis {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedPaneNode {
    Leaf {
        pane: usize,
    },
    Split {
        id: usize,
        #[serde(default)]
        axis: SavedAxis,
        left: Box<SavedPaneNode>,
        right: Box<SavedPaneNode>,
    },
}

impl Default for SavedPaneNode {
    fn default() -> Self {
        Self::Leaf { pane: 0 }
    }
}

impl SavedPaneNode {
    fn validate(self, panes: &HashSet<usize>) -> Option<Self> {
        match self {
            Self::Leaf { pane } => panes.contains(&pane).then_some(Self::Leaf { pane }),
            Self::Split {
                id,
                axis,
                left,
                right,
            } => match (left.validate(panes), right.validate(panes)) {
                (Some(left), Some(right)) => Some(Self::Split {
                    id,
                    axis,
                    left: Box::new(left),
                    right: Box::new(right),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedModelChat {
    pub model: Option<AgentModel>,
    pub shob_session_id: Option<String>,
    pub conversation: String,
    /// Conversation name reported by the agent for this model.
    pub agent_title: Option<String>,
    /// Conversation name derived from this model's first prompt.
    pub prompt_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedChat {
    #[serde(default = "default_true")]
    pub fast_mode: bool,
    /// Exact OMP reasoning effort; absent means the Fast/Deep preset.
    pub thinking_override: Option<String>,
    pub thinking_by_model: std::collections::HashMap<String, String>,
    pub selected_model: Option<AgentModel>,
    /// Provider that produced the first assistant text in this conversation.
    pub first_response_provider: Option<String>,
    /// True when `selected_model` is a deliberate choice, so `None` can mean
    /// "Automatic (shob)" instead of "never picked one".
    #[serde(default)]
    pub model_explicit: bool,
    /// Conversation name reported by the agent itself, when it generates one.
    pub agent_title: Option<String>,
    /// Name derived from the first prompt, shown until the agent reports one.
    pub prompt_title: Option<String>,
    pub shob_session_id: Option<String>,
    pub conversation_key: Option<String>,
    #[serde(default)]
    pub workspace_override: Option<PathBuf>,
    pub models: Vec<SavedModelChat>,
}

impl SavedChat {
    pub fn valid_conversation_key(&self) -> Option<&str> {
        self.conversation_key.as_deref().filter(|key| {
            !key.is_empty()
                && key
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
        })
    }
}

impl Default for SavedChat {
    fn default() -> Self {
        Self {
            fast_mode: true,
            thinking_override: None,
            thinking_by_model: Default::default(),
            selected_model: None,
            first_response_provider: None,
            model_explicit: false,
            agent_title: None,
            prompt_title: None,
            shob_session_id: None,
            conversation_key: None,
            workspace_override: None,
            models: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPane {
    pub id: usize,
    pub uid: String,
    pub title: String,
    pub display_title: String,
    pub cwd: Option<PathBuf>,
    pub draft: String,
    pub agent_draft: String,
    pub queued_messages: Vec<String>,
    pub command_history: Vec<String>,
    pub chat_active: bool,
    pub one_mode: bool,
    pub regular_focus: Option<usize>,
    pub tool_panel_owner: Option<usize>,
    pub tool_panel_changes: bool,
    pub tool_panel_tab: Option<usize>,
    pub tool_panel_hidden: bool,
    pub tool_panel_chooser_open: bool,
    pub tool_panel_file: Option<PathBuf>,
    pub tool_panel_chat_ratio: Option<f32>,
    pub files: Option<crate::files_surface::SavedFiles>,
    pub chat: SavedChat,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub id: usize,
    pub uid: String,
    pub title: String,
    pub display_title: String,
    pub logo: String,
    pub maximized_pane_id: Option<usize>,
    pub focused_pane: Option<usize>,
    pub tree: SavedPaneNode,
    pub panes: Vec<SavedPane>,
}

impl Default for SavedSession {
    fn default() -> Self {
        Self {
            id: 0,
            uid: String::new(),
            title: "terminal 1".to_string(),
            display_title: "terminal 1".to_string(),
            logo: String::new(),
            maximized_pane_id: None,
            focused_pane: None,
            tree: SavedPaneNode::default(),
            panes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedProject {
    pub id: usize,
    pub path: PathBuf,
    pub name: String,
    #[serde(default = "default_true")]
    pub expanded: bool,
    pub active_session: usize,
    pub sessions: Vec<SavedSession>,
}

impl Default for SavedProject {
    fn default() -> Self {
        Self {
            id: 0,
            path: PathBuf::new(),
            name: String::new(),
            expanded: true,
            active_session: 0,
            sessions: Vec::new(),
        }
    }
}

/// Sidebar-only thread state. It lives outside `SavedSession` so the v2
/// session shape and its migration stay untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSidebarThread {
    pub session_uid: String,
    pub pinned: bool,
    /// Lower values appear first among pinned threads; swaps persist reordering.
    pub pin_order: Option<u64>,
    pub settled: bool,
    pub snoozed_until_ms: Option<u64>,
    pub archived: bool,
    pub parked_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWorkspace {
    pub version: u32,
    pub active_project: Option<PathBuf>,
    pub projects: Vec<SavedProject>,
    #[serde(default)]
    pub threads: Vec<SavedSidebarThread>,
}

impl Default for SavedWorkspace {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            active_project: None,
            projects: Vec::new(),
            threads: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl SavedWorkspace {
    /// Highest numeric id in the file, used to keep `AppState::next_id` from
    /// reissuing an id that is still referenced by a restored split tree.
    pub fn max_id(&self) -> usize {
        let mut max = 0;
        for project in &self.projects {
            max = max.max(project.id);
            for session in &project.sessions {
                max = max.max(session.id);
                for pane in &session.panes {
                    max = max.max(pane.id);
                }
                max = max.max(max_split_id(&session.tree));
            }
        }
        max
    }

    /// Remove references that no longer resolve, collapse empty splits, and
    /// repair indices. Called after every load.
    pub fn normalize(&mut self) {
        self.version = CURRENT_VERSION;
        self.projects
            .retain(|project| !project.path.as_os_str().is_empty());
        self.projects.dedup_by(|a, b| a.path == b.path);
        let mut seen_thread_uids = HashSet::new();
        self.threads.retain(|thread| {
            !thread.session_uid.is_empty() && seen_thread_uids.insert(thread.session_uid.clone())
        });
        // Files written before pin ordering existed still need a stable order.
        let mut next_pin_order = self
            .threads
            .iter()
            .filter_map(|thread| thread.pin_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        for thread in &mut self.threads {
            if !thread.pinned {
                thread.pin_order = None;
            } else if thread.pin_order.is_none() {
                thread.pin_order = Some(next_pin_order);
                next_pin_order = next_pin_order.saturating_add(1);
            }
        }

        for project in &mut self.projects {
            if project.name.is_empty() {
                project.name = project
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
            }

            let mut seen_pane_ids = HashSet::new();
            for session in &mut project.sessions {
                if session.uid.is_empty() {
                    session.uid = paths::uid("s");
                }
                session.panes.retain(|pane| {
                    if pane.id == 0 || !seen_pane_ids.insert(pane.id) {
                        return false;
                    }
                    true
                });
                for pane in &mut session.panes {
                    if pane.uid.is_empty() {
                        pane.uid = paths::uid("p");
                    }
                    if pane.title.is_empty() {
                        pane.title = "terminal".to_string();
                    }
                    if pane.display_title.is_empty() {
                        pane.display_title = pane.title.clone();
                    }
                    if pane.command_history.len() > MAX_COMMAND_HISTORY {
                        let skip = pane.command_history.len() - MAX_COMMAND_HISTORY;
                        pane.command_history.drain(..skip);
                    }
                }

                let pane_ids: HashSet<usize> = session.panes.iter().map(|pane| pane.id).collect();
                if pane_ids.is_empty() {
                    session.tree = SavedPaneNode::default();
                } else {
                    session.tree = session.tree.clone().validate(&pane_ids).unwrap_or_else(|| {
                        SavedPaneNode::Leaf {
                            pane: session.panes[0].id,
                        }
                    });
                    if let Some(maximized) = session.maximized_pane_id
                        && !pane_ids.contains(&maximized)
                    {
                        session.maximized_pane_id = None;
                    }
                    if session
                        .focused_pane
                        .is_some_and(|id| !pane_ids.contains(&id))
                    {
                        session.focused_pane = None;
                    }
                    if session.focused_pane.is_none() {
                        session.focused_pane = Some(session.panes[0].id);
                    }
                }
            }
            project.sessions.retain(|session| !session.panes.is_empty());
            if project.sessions.is_empty() {
                project.active_session = 0;
            } else {
                project.active_session = project.active_session.min(project.sessions.len() - 1);
            }
        }

        let live_thread_uids: HashSet<String> = self
            .projects
            .iter()
            .flat_map(|project| project.sessions.iter().map(|session| session.uid.clone()))
            .collect();
        self.threads
            .retain(|thread| live_thread_uids.contains(&thread.session_uid));

        let active = self.active_project.as_ref().and_then(|path| {
            self.projects
                .iter()
                .position(|project| &project.path == path)
        });
        if active.is_none() {
            self.active_project = self.projects.first().map(|project| project.path.clone());
        }
    }
}

fn max_split_id(node: &SavedPaneNode) -> usize {
    match node {
        SavedPaneNode::Leaf { .. } => 0,
        SavedPaneNode::Split {
            id, left, right, ..
        } => (*id).max(max_split_id(left)).max(max_split_id(right)),
    }
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct LegacyProject {
    #[serde(default)]
    path: PathBuf,
    #[serde(default)]
    name: String,
    #[serde(default = "default_true")]
    expanded: bool,
}

#[derive(Deserialize, Default)]
struct LegacyWorkspace {
    #[serde(default)]
    projects: Vec<LegacyProject>,
    #[serde(default)]
    active_project: Option<PathBuf>,
}

fn migrate_legacy(legacy: LegacyWorkspace) -> SavedWorkspace {
    let mut next_id = 1usize;
    let mut projects = Vec::new();
    for project in legacy.projects {
        if project.path.as_os_str().is_empty() {
            continue;
        }
        let project_id = next_id;
        next_id += 1;
        let session_id = next_id;
        next_id += 1;
        let pane_id = next_id;
        next_id += 1;

        let name = if project.name.is_empty() {
            project
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string())
        } else {
            project.name
        };
        let title = "terminal 1".to_string();
        projects.push(SavedProject {
            id: project_id,
            path: project.path.clone(),
            name,
            expanded: project.expanded,
            active_session: 0,
            sessions: vec![SavedSession {
                id: session_id,
                uid: paths::uid("s"),
                title: title.clone(),
                display_title: title,
                logo: String::new(),
                maximized_pane_id: None,
                focused_pane: Some(pane_id),
                tree: SavedPaneNode::Leaf { pane: pane_id },
                panes: vec![SavedPane {
                    id: pane_id,
                    uid: paths::uid("p"),
                    title: "terminal".to_string(),
                    display_title: "terminal".to_string(),
                    cwd: Some(project.path),
                    chat: SavedChat::default(),
                    ..Default::default()
                }],
            }],
        });
    }

    SavedWorkspace {
        version: CURRENT_VERSION,
        active_project: legacy.active_project,
        projects,
        threads: Vec::new(),
    }
}

fn backup_file(path: &Path) {
    let Some(file_name) = path.file_name() else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup_dir = paths::backups_dir();
    if fs::create_dir_all(&backup_dir).is_err() {
        return;
    }
    let dest = backup_dir.join(format!("{}-v1-{stamp}.json", file_name.to_string_lossy()));
    if dest.exists() {
        return;
    }
    match fs::copy(path, &dest) {
        Ok(_) => eprintln!("backed up legacy workspace to {}", dest.display()),
        Err(error) => eprintln!("failed to back up legacy workspace: {error}"),
    }
}

/// Parse a workspace file, converting v1 files to the current schema.
pub fn load_from_text(text: &str) -> SavedWorkspace {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return SavedWorkspace::default(),
    };
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let mut workspace = if version < u64::from(CURRENT_VERSION) {
        serde_json::from_value::<LegacyWorkspace>(value)
            .map(migrate_legacy)
            .unwrap_or_default()
    } else {
        serde_json::from_value::<SavedWorkspace>(value).unwrap_or_default()
    };
    workspace.normalize();
    workspace
}

pub fn load(path: &Path) -> SavedWorkspace {
    let Ok(text) = fs::read_to_string(path) else {
        return SavedWorkspace::default();
    };
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64))
        .unwrap_or(1);
    if version < u64::from(CURRENT_VERSION) {
        backup_file(path);
    }
    load_from_text(&text)
}

pub fn save_atomic(path: &Path, state: &SavedWorkspace) -> io::Result<()> {
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    save_locked(path, state)
}

pub fn save_if_current(
    path: &Path,
    state: &SavedWorkspace,
    generation: &AtomicU64,
    expected: u64,
) -> io::Result<bool> {
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if generation.load(Ordering::SeqCst) != expected {
        return Ok(false);
    }
    save_locked(path, state)?;
    Ok(true)
}

fn save_locked(path: &Path, state: &SavedWorkspace) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temp, bytes)?;
    if let Err(error) = paths::atomic_replace(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scrollback store
// ---------------------------------------------------------------------------

fn scrollback_path_in(dir: &Path, uid: &str) -> PathBuf {
    dir.join(format!("{uid}.txt"))
}

/// Keep the most recent bytes without splitting a UTF-8 character, then trim
/// to the first complete line.
fn cap_scrollback(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    match tail.find('\n') {
        Some(index) => &tail[index + 1..],
        None => tail,
    }
}

pub fn write_scrollback(uid: &str, text: &str) -> io::Result<()> {
    write_scrollback_in(&paths::sessions_dir(), uid, text)
}

fn write_scrollback_in(dir: &Path, uid: &str, text: &str) -> io::Result<()> {
    let text = cap_scrollback(text, MAX_SCROLLBACK_BYTES);
    if text.trim().is_empty() {
        let _ = fs::remove_file(scrollback_path_in(dir, uid));
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let path = scrollback_path_in(dir, uid);
    let temp = path.with_extension(format!("txt.{}.tmp", std::process::id()));
    fs::write(&temp, text)?;
    if let Err(error) = paths::atomic_replace(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

pub fn read_scrollback(uid: &str) -> Option<String> {
    read_scrollback_in(&paths::sessions_dir(), uid)
}

fn read_scrollback_in(dir: &Path, uid: &str) -> Option<String> {
    fs::read_to_string(scrollback_path_in(dir, uid))
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// Delete scrollback files whose panes no longer exist in the workspace.
pub fn prune_stale_scrollbacks(live_uids: &HashSet<String>) {
    prune_stale_scrollbacks_in(&paths::sessions_dir(), live_uids);
}

/// A deletion intent remains on disk until the workspace and pane files have
/// both been updated. It is replayed before restoring a workspace after a crash.
#[derive(Debug, Serialize, Deserialize)]
pub struct PendingDeletion {
    pub session_uid: String,
    pub pane_uids: Vec<String>,
    #[serde(default)]
    pub shob_session_ids: Vec<String>,
}

fn safe_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn deletions_dir() -> PathBuf {
    paths::data_dir().join("pending-deletions")
}

pub fn record_deletion(deletion: &PendingDeletion) -> io::Result<PathBuf> {
    if !safe_uid(&deletion.session_uid) || deletion.pane_uids.iter().any(|uid| !safe_uid(uid)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session or pane identifier",
        ));
    }
    let dir = deletions_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", deletion.session_uid));
    let temp = path.with_extension("tmp");
    fs::write(
        &temp,
        serde_json::to_vec(deletion).map_err(io::Error::other)?,
    )?;
    paths::atomic_replace(&temp, &path)?;
    Ok(path)
}

pub fn finish_deletion(path: &Path, deletion: &PendingDeletion) -> io::Result<()> {
    crate::shob_storage::delete_sessions(&deletion.shob_session_ids)?;
    for uid in &deletion.pane_uids {
        if !safe_uid(uid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid pane identifier",
            ));
        }
        crate::conversations::delete_pane(uid)?;
        match fs::remove_file(scrollback_path_in(&paths::sessions_dir(), uid)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    fs::remove_file(path)
}

/// Replay pending deletions against the saved workspace before loading it.
pub fn retry_pending_deletions(workspace_path: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(deletions_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let deletion: PendingDeletion =
            serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
        if !safe_uid(&deletion.session_uid) || deletion.pane_uids.iter().any(|uid| !safe_uid(uid)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid pending deletion",
            ));
        }
        if workspace_path.exists() {
            let text = fs::read_to_string(workspace_path)?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(io::Error::other)?;
            if value.get("version").and_then(|v| v.as_u64()).unwrap_or(1)
                >= u64::from(CURRENT_VERSION)
            {
                let mut workspace: SavedWorkspace =
                    serde_json::from_value(value).map_err(io::Error::other)?;
                for project in &mut workspace.projects {
                    project
                        .sessions
                        .retain(|session| session.uid != deletion.session_uid);
                    project.active_session = project
                        .active_session
                        .min(project.sessions.len().saturating_sub(1));
                }
                save_atomic(workspace_path, &workspace)?;
            }
        }
        finish_deletion(&path, &deletion)?;
    }
    Ok(())
}

pub fn hide_pending_deletions(workspace: &mut SavedWorkspace) -> io::Result<()> {
    let entries = match fs::read_dir(deletions_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let deletion: PendingDeletion =
            serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
        for project in &mut workspace.projects {
            project
                .sessions
                .retain(|session| session.uid != deletion.session_uid);
            project.active_session = project
                .active_session
                .min(project.sessions.len().saturating_sub(1));
        }
    }
    Ok(())
}

fn prune_stale_scrollbacks_in(dir: &Path, live_uids: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
            continue;
        }
        if !live_uids.contains(stem) {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn max_scrollback_lines() -> usize {
    MAX_SCROLLBACK_LINES
}

#[cfg(test)]
#[path = "workspace_state/tests.rs"]
mod tests;
