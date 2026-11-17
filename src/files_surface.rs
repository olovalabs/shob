use gpui::{
    App, AppContext, Context, Entity, EventEmitter, IntoElement, KeyDownEvent, MouseButton, Render,
    Subscription, Task, WeakEntity, Window, div, prelude::*, px, relative, svg, uniform_list,
};
use gpui_component::{
    ActiveTheme as _,
    input::{Input, InputEvent, InputState},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_ENTRIES: usize = 20_000;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TABS: usize = 8;
const MAX_GIT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedFiles {
    pub active: bool,
    expanded: BTreeSet<PathBuf>,
    tabs: Vec<SavedFile>,
    selected: Option<PathBuf>,
    git: bool,
    commit_message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SavedFile {
    path: PathBuf,
    draft: Option<String>,
    baseline: Option<String>,
    cursor: (u32, u32),
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    directory: bool,
    depth: usize,
}

#[derive(Clone)]
struct Change {
    path: PathBuf,
    status: String,
    staged: bool,
    unstaged: bool,
}

struct Repository {
    root: PathBuf,
    branch: String,
    changes: Vec<Change>,
    statuses: HashMap<PathBuf, String>,
}
struct Editor {
    path: PathBuf,
    input: Entity<InputState>,
    baseline: String,
    saving: bool,
    dirty: bool,
    _subscription: Subscription,
}

pub struct Changed;
pub struct FilesSurface {
    root: PathBuf,
    owner: WeakEntity<crate::AppState>,
    pane_id: usize,
    expanded: BTreeSet<PathBuf>,
    entries: Vec<Entry>,
    repository: Option<Repository>,
    editors: Vec<Editor>,
    selected: Option<PathBuf>,
    pending: Vec<SavedFile>,
    git: bool,
    diff: Option<Entity<InputState>>,
    diff_path: Option<PathBuf>,
    message: String,
    commit: Entity<InputState>,
    _commit_subscription: Subscription,
    refresh_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    loading: Option<SavedFile>,
    operation: Option<Task<()>>,
}
impl EventEmitter<Changed> for FilesSurface {}

fn valid_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn tree(root: &Path, expanded: &BTreeSet<PathBuf>) -> Result<(Vec<Entry>, bool), String> {
    fn visit(
        root: &Path,
        directory: &Path,
        expanded: &BTreeSet<PathBuf>,
        rows: &mut Vec<Entry>,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<bool, String> {
        if depth >= 64 {
            return Ok(true);
        }
        let mut children = Vec::new();
        let mut truncated = false;
        for child in std::fs::read_dir(root.join(directory)).map_err(|e| e.to_string())? {
            if *remaining == 0 {
                truncated = true;
                break;
            }
            let child = child.map_err(|e| e.to_string())?;
            if child.file_name() == ".git" {
                continue;
            }
            *remaining -= 1;
            let directory = child.file_type().map_err(|e| e.to_string())?.is_dir();
            children.push(Entry {
                path: child.path().strip_prefix(root).unwrap().to_path_buf(),
                directory,
                depth,
            });
        }
        children.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.path.cmp(&b.path))
        });
        for child in children {
            if rows.len() >= MAX_ENTRIES {
                return Ok(true);
            }
            let descend = child.directory && expanded.contains(&child.path);
            let path = child.path.clone();
            rows.push(child);
            if descend {
                // A directory may disappear or become inaccessible during a refresh.
                truncated |=
                    visit(root, &path, expanded, rows, depth + 1, remaining).unwrap_or(true);
            }
        }
        Ok(truncated)
    }
    let mut rows = Vec::new();
    let mut remaining = MAX_ENTRIES;
    let truncated = visit(root, Path::new(""), expanded, &mut rows, 0, &mut remaining)?;
    Ok((rows, truncated))
}

fn git(root: &Path, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let mut child = Command::new("git")
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--literal-pathspecs")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .unwrap()
        .take(MAX_GIT_BYTES + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > MAX_GIT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Git output exceeded the 4 MiB limit or could not be read.".into());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(
            "Git command failed. Check the repository, Git identity, and index lock.".into(),
        );
    }
    String::from_utf8(bytes).map_err(|_| "Git returned a path that is not UTF-8.".into())
}
fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    git(
        root,
        &args.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
    )
}
fn repository(directory: &Path) -> Result<Repository, String> {
    let root = PathBuf::from(
        run(directory, &["rev-parse", "--show-toplevel"])?.trim_end_matches(['\r', '\n']),
    );
    let branch = run(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .unwrap_or_else(|_| "Detached HEAD".into());
    let status = run(
        &root,
        &[
            "status",
            "--no-renames",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    let changes = parse_status(&status)?;
    let mut statuses: HashMap<_, _> = changes
        .iter()
        .map(|c| (root.join(&c.path), c.status.clone()))
        .collect();
    for change in &changes {
        for parent in change.path.ancestors().skip(1).take(64) {
            if statuses.len() >= MAX_ENTRIES * 2 || parent.as_os_str().is_empty() {
                break;
            }
            statuses
                .entry(root.join(parent))
                .or_insert_with(|| "M".into());
        }
    }
    Ok(Repository {
        root,
        statuses,
        branch: branch.trim().into(),
        changes,
    })
}
fn parse_status(status: &str) -> Result<Vec<Change>, String> {
    let mut records = status.split('\0').filter(|s| !s.is_empty());
    let mut changes = Vec::new();
    while let Some(record) = records.next() {
        if changes.len() >= MAX_ENTRIES {
            return Err("Too many Git changes to display (20,000 limit).".into());
        }
        let bytes = record.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            return Err("Invalid Git status response.".into());
        }
        let path = PathBuf::from(&record[3..]);
        if !valid_relative(&path) {
            return Err("Invalid Git path.".into());
        }
        changes.push(Change {
            path,
            status: record[..2].into(),
            staged: bytes[0] != b' ' && bytes[0] != b'?',
            unstaged: bytes[1] != b' ',
        });
        if bytes[..2].iter().any(|c| matches!(c, b'R' | b'C')) {
            records.next();
        }
    }
    Ok(changes)
}
fn read_text(root: &Path, path: &Path) -> Result<String, String> {
    if !valid_relative(path) {
        return Err("Invalid file path.".into());
    }
    if !std::fs::metadata(root.join(path))
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("Only regular text files can be edited.".into());
    }
    let file = std::fs::File::open(root.join(path)).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Only regular text files can be edited.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err("Editing is limited to files up to 1 MiB.".into());
    }
    if bytes.contains(&0) {
        return Err("Binary files cannot be edited.".into());
    }
    String::from_utf8(bytes).map_err(|_| "File is not UTF-8 text.".into())
}
fn save(root: &Path, path: &Path, baseline: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_FILE_BYTES {
        return Err("Editing is limited to files up to 1 MiB.".into());
    }
    if read_text(root, path)? != baseline {
        return Err(
            "File changed on disk. Your draft is kept; resolve the external change before saving."
                .into(),
        );
    }
    let destination = std::fs::canonicalize(root.join(path)).map_err(|e| e.to_string())?;
    let permissions = std::fs::metadata(&destination)
        .map_err(|e| e.to_string())?
        .permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(destination.parent().unwrap())
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    temporary
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    temporary
        .as_file()
        .set_permissions(permissions)
        .map_err(|e| e.to_string())?;
    temporary.as_file().sync_all().map_err(|e| e.to_string())?;
    temporary.persist(destination).map_err(|e| e.to_string())?;
    Ok(())
}

impl FilesSurface {
    pub fn new(
        root: PathBuf,
        saved: SavedFiles,
        owner: WeakEntity<crate::AppState>,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let commit = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Commit message")
                .validate(|text, _| text.len() <= 16 * 1024)
                .default_value(saved.commit_message)
        });
        let subscription = cx.subscribe(&commit, |_, _, _: &InputEvent, cx| {
            cx.emit(Changed);
        });
        let mut this = Self {
            root,
            owner,
            pane_id,
            expanded: saved
                .expanded
                .into_iter()
                .filter(|p| valid_relative(p))
                .take(MAX_ENTRIES)
                .collect(),
            entries: Vec::new(),
            repository: None,
            editors: Vec::new(),
            selected: saved.selected,
            pending: saved
                .tabs
                .into_iter()
                .filter(|f| valid_relative(&f.path))
                .take(MAX_TABS)
                .collect(),
            git: saved.git,
            diff: None,
            diff_path: None,
            message: "Loading files…".into(),
            commit,
            _commit_subscription: subscription,
            refresh_task: None,
            open_task: None,
            loading: None,
            operation: None,
        };
        this.refresh(cx);
        cx.defer_in(window, |this, window, cx| this.restore_next(window, cx));
        this
    }
    pub fn prepare_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.operation.is_some()
            || self.editors.iter().any(|e| e.dirty)
            || self
                .pending
                .iter()
                .chain(self.loading.iter())
                .any(|f| f.draft.is_some())
        {
            self.message =
                "Save or discard file drafts and wait for pending operations before closing Files."
                    .into();
            cx.notify();
            return false;
        }
        true
    }
    pub fn snapshot(&self, cx: &App) -> SavedFiles {
        let mut tabs: Vec<_> = self
            .editors
            .iter()
            .map(|editor| {
                let text = editor.input.read(cx).value().to_string();
                let dirty = text != editor.baseline;
                SavedFile {
                    path: editor.path.clone(),
                    draft: dirty.then_some(text),
                    baseline: dirty.then(|| editor.baseline.clone()),
                    cursor: {
                        let p = editor.input.read(cx).cursor_position();
                        (p.line, p.character)
                    },
                }
            })
            .collect();
        tabs.extend(self.pending.iter().cloned());
        tabs.extend(self.loading.iter().cloned());
        SavedFiles {
            active: true,
            expanded: self.expanded.clone(),
            tabs,
            selected: self.selected.clone(),
            git: self.git,
            commit_message: self.commit.read(cx).value().to_string(),
        }
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(Changed);
        cx.notify();
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.message == "Loading files…" {
            self.message.clear();
        }
        let root = self.root.clone();
        let expanded = self.expanded.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (tree, repo) = cx
                .background_executor()
                .spawn(async move { (tree(&root, &expanded), repository(&root)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.message = match tree {
                    Ok((entries, truncated)) => {
                        this.entries = entries;
                        if truncated {
                            "Tree limit reached or a folder could not be read.".into()
                        } else {
                            this.message.clone()
                        }
                    }
                    Err(error) => error,
                };
                match repo {
                    Ok(repo) => this.repository = Some(repo),
                    Err(error) => {
                        this.repository = None;
                        if this.git {
                            this.message = error;
                        }
                    }
                }
                cx.notify();
            });
        }));
    }
    fn restore_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending.is_empty() {
            let file = self.pending.remove(0);
            self.open(file, false, window, cx);
        }
    }
    fn open(
        &mut self,
        saved: SavedFile,
        select: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading.is_some() {
            return;
        }
        if saved.path.to_str().is_none() {
            self.message = "Editing requires a UTF-8 file path.".into();
            cx.notify();
            return;
        }
        if self.editors.iter().any(|e| e.path == saved.path) {
            self.selected = Some(saved.path);
            self.diff = None;
            if select || self.owns_focus(cx) {
                self.focus_selected(window, cx);
            }
            self.changed(cx);
            if !select {
                self.restore_next(window, cx);
            }
            return;
        }
        if self.editors.len() >= MAX_TABS {
            self.message = "Close a saved tab before opening more files (8 tab limit).".into();
            cx.notify();
            return;
        }
        let root = self.root.clone();
        let target = window.window_handle();
        let path = saved.path.clone();
        self.loading = Some(saved.clone());
        let cursor = saved.cursor;
        self.message = "Loading file…".into();
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let (Some(draft), Some(baseline)) = (saved.draft, saved.baseline) {
                        if draft.len() <= MAX_FILE_BYTES && baseline.len() <= MAX_FILE_BYTES {
                            return Ok((baseline, draft));
                        }
                        return Err("Restored draft exceeds the editor limit.".into());
                    }
                    read_text(&root, &saved.path).map(|text| (text.clone(), text))
                })
                .await;
            let _ = target.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.loading = None;
                    match result {
                        Ok((baseline, text)) => {
                            let dirty = text != baseline;
                            let language = crate::file_preview::language(&path.to_string_lossy());
                            let input = cx.new(|cx| {
                                InputState::new(window, cx)
                                    .code_editor(language)
                                    .line_number(true)
                                    .soft_wrap(false)
                                    .max_bytes(MAX_FILE_BYTES)
                                    .default_value(text)
                            });
                            let prior_focus = window.focused(cx);
                            input.update(cx, |input, cx| {
                                input.set_cursor_position(
                                    gpui_component::input::Position::new(cursor.0, cursor.1),
                                    window,
                                    cx,
                                )
                            });
                            let take_focus = select
                                || (this.owns_focus(cx) && this.selected.as_ref() == Some(&path));
                            if !take_focus {
                                if let Some(focus) = prior_focus {
                                    window.focus(&focus, cx);
                                } else {
                                    window.blur(cx);
                                }
                            }
                            let subscription =
                                cx.subscribe(&input, |_, input, event: &InputEvent, cx| {
                                    let target = cx.entity().downgrade();
                                    let input = input.clone();
                                    let changed = matches!(event, InputEvent::Change);
                                    cx.defer(move |cx| {
                                        let _ = target.update(cx, |this, cx| {
                                            if changed
                                                && let Some(editor) = this
                                                    .editors
                                                    .iter_mut()
                                                    .find(|e| e.input == input)
                                            {
                                                editor.dirty = input.read(cx).text()
                                                    != editor.baseline.as_str();
                                            }
                                            this.changed(cx);
                                        });
                                    });
                                });
                            if select || this.selected.is_none() {
                                this.selected = Some(path.clone());
                            }
                            this.editors.push(Editor {
                                path,
                                input,
                                baseline,
                                saving: false,
                                dirty,
                                _subscription: subscription,
                            });
                            this.diff = None;
                            this.message.clear();
                            this.changed(cx);
                        }
                        Err(error) => {
                            this.message = error;
                            cx.notify();
                        }
                    }
                    this.restore_next(window, cx);
                });
            });
        }));
    }
    fn owns_focus(&self, cx: &App) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            let owner = owner.read(cx);
            owner.focused_pane_id == Some(self.pane_id)
                && owner.pane_ref(self.pane_id).is_some_and(|p| p.files_active)
        })
    }
    fn focus_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self
            .editors
            .iter()
            .find(|e| Some(&e.path) == self.selected.as_ref())
        {
            editor.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }
    fn save_selected(&mut self, cx: &mut Context<Self>) {
        if self.operation.is_some() || self.diff.is_some() {
            return;
        }
        let Some(editor) = self
            .editors
            .iter_mut()
            .find(|e| Some(&e.path) == self.selected.as_ref())
        else {
            return;
        };
        let text = editor.input.read(cx).value().to_string();
        let baseline = editor.baseline.clone();
        let path = editor.path.clone();
        let root = self.root.clone();
        editor.saving = true;
        self.operation = Some(cx.spawn(async move |this, cx| {
            let saved_text = text.clone();
            let saved_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { save(&root, &saved_path, &baseline, &saved_text) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(editor) = this.editors.iter_mut().find(|e| e.path == path) {
                    editor.saving = false;
                    if result.is_ok() {
                        editor.baseline = text;
                        editor.dirty = editor.input.read(cx).text() != editor.baseline.as_str();
                    }
                }
                this.operation = None;
                this.message = result.err().unwrap_or_else(|| "Saved".into());
                this.refresh(cx);
                this.changed(cx);
            });
        }));
        cx.notify();
    }
    fn git_action(&mut self, args: Vec<std::ffi::OsString>, cx: &mut Context<Self>) {
        if self.operation.is_some() {
            return;
        }
        let Some(repo) = &self.repository else {
            return;
        };
        let root = repo.root.clone();
        self.message = "Running Git…".into();
        self.operation = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    git(
                        &root,
                        &args.iter().map(|s| s.as_os_str()).collect::<Vec<_>>(),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.operation = None;
                this.message = result
                    .err()
                    .unwrap_or_else(|| "Git operation completed".into());
                this.refresh(cx);
                this.changed(cx);
            });
        }));
        cx.notify();
    }
    fn show_diff(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading.is_some() {
            return;
        }
        let Some(repo) = &self.repository else {
            return;
        };
        let root = repo.root.clone();
        self.diff_path = Some(change.path.clone());
        let target = window.window_handle();
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if change.status == "??" {
                        let text = read_text(&root, &change.path)?;
                        let lines: String = text.lines().map(|line| format!("+{line}\n")).collect();
                        return Ok(format!(
                            "--- /dev/null\n+++ b/{}\n@@ -0,0 +1,{} @@\n{lines}",
                            change.path.display(),
                            text.lines().count()
                        ));
                    }
                    let path = change.path.as_os_str();
                    let staged = git(
                        &root,
                        &[
                            "diff".as_ref(),
                            "--cached".as_ref(),
                            "--no-ext-diff".as_ref(),
                            "--no-textconv".as_ref(),
                            "--no-color".as_ref(),
                            "--".as_ref(),
                            path,
                        ],
                    )?;
                    let working = git(
                        &root,
                        &[
                            "diff".as_ref(),
                            "--no-ext-diff".as_ref(),
                            "--no-textconv".as_ref(),
                            "--no-color".as_ref(),
                            "--".as_ref(),
                            path,
                        ],
                    )?;
                    Ok(format!(
                        "Staged changes\n{staged}\nWorking tree changes\n{working}"
                    ))
                })
                .await;
            let _ = target.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(text) => {
                            this.diff = Some(cx.new(|cx| {
                                InputState::new(window, cx)
                                    .code_editor("diff")
                                    .line_number(true)
                                    .soft_wrap(false)
                                    .read_only(true)
                                    .default_value(text)
                            }))
                        }
                        Err(error) => this.message = error,
                    }
                    cx.notify();
                });
            });
        }));
    }
}

impl Render for FilesSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.owner.upgrade().unwrap().read(cx).tokens(cx);
        let weak = cx.entity().downgrade();
        let count = if self.git {
            self.repository.as_ref().map_or(0, |r| r.changes.len())
        } else {
            self.entries.len()
        };
        let rows = uniform_list("files-tree", count, move |range, _, cx| {
            let Some(entity) = weak.upgrade() else {
                return Vec::new();
            };
            let this = entity.read(cx);
            let tokens = this.owner.upgrade().unwrap().read(cx).tokens(cx);
            range
                .map(|index| {
                    let (path, directory, depth, status, staged, unstaged) = if this.git {
                        let c = &this.repository.as_ref().unwrap().changes[index];
                        (
                            c.path.clone(),
                            false,
                            0,
                            c.status.clone(),
                            c.staged,
                            c.unstaged,
                        )
                    } else {
                        let e = &this.entries[index];
                        let status = this
                            .repository
                            .as_ref()
                            .and_then(|r| r.statuses.get(&this.root.join(&e.path)))
                            .cloned()
                            .unwrap_or_default();
                        (e.path.clone(), e.directory, e.depth, status, false, false)
                    };
                    let change_color = if status.contains('D') {
                        tokens.block_failure
                    } else if status.contains('A') || status == "??" {
                        tokens.block_success
                    } else if status.trim().is_empty() {
                        tokens.text_primary
                    } else {
                        tokens.block_warning
                    };
                    let expanded = this.expanded.contains(&path);
                    let label = if this.git {
                        path.to_string_lossy().into_owned()
                    } else {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    };
                    let selected = this.selected.as_ref() == Some(&path) && !this.git;
                    let click_path = path.clone();
                    let git_mode = this.git;
                    let row_entity = entity.clone();
                    let mut row = div()
                        .id(("file-row", index))
                        .h(px(26.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(8.0 + depth as f32 * 16.0))
                        .pr(px(8.0))
                        .text_size(px(12.0))
                        .text_color(tokens.text_primary)
                        .when(selected, |s| s.bg(tokens.card_hover))
                        .cursor_pointer()
                        .hover(|s| s.bg(tokens.card_hover))
                        .child(
                            svg()
                                .path(if directory {
                                    if expanded {
                                        "zed_file_icons/chevron_down.svg"
                                    } else {
                                        "zed_file_icons/chevron_right.svg"
                                    }
                                } else {
                                    "zed_file_icons/chevron_right.svg"
                                })
                                .when(!directory, |s| s.opacity(0.0))
                                .size(px(12.0))
                                .text_color(tokens.text_muted),
                        )
                        .child(
                            svg()
                                .path(if directory {
                                    if expanded {
                                        "zed_file_icons/folder_open.svg".to_owned()
                                    } else {
                                        "zed_file_icons/folder.svg".to_owned()
                                    }
                                } else {
                                    crate::file_icons::icon_for_path(&path.to_string_lossy())
                                        .to_string()
                                })
                                .size(px(16.0))
                                .flex_shrink_0()
                                .text_color(tokens.text_secondary),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(change_color)
                                .child(label),
                        )
                        .child(div().text_color(change_color).child(status))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            cx.stop_propagation();
                            row_entity.update(cx, |this, cx| {
                                if directory {
                                    if !this.expanded.remove(&click_path) {
                                        if this.expanded.len() >= MAX_ENTRIES || click_path.to_str().is_none() {
                                            this.message = "Folder expansion limit reached or folder name is not UTF-8.".into();
                                            cx.notify();
                                            return;
                                        }
                                        this.expanded.insert(click_path.clone());
                                    }
                                    this.refresh(cx);
                                    this.changed(cx);
                                } else if git_mode {
                                    if let Some(change) = this
                                        .repository
                                        .as_ref()
                                        .and_then(|r| {
                                            r.changes.iter().find(|c| c.path == click_path)
                                        })
                                        .cloned()
                                    {
                                        this.show_diff(change, window, cx);
                                    }
                                } else {
                                    this.open(
                                        SavedFile {
                                            path: click_path.clone(),
                                            ..Default::default()
                                        },
                                        true,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        });
                    for (enabled, label, stage) in [(unstaged, "+", true), (staged, "−", false)] {
                        if !enabled {
                            continue;
                        }
                        let action_path = path.clone();
                        let action_entity = entity.clone();
                        row = row.child(
                            div()
                                .id((if stage { "stage" } else { "unstage" }, index))
                                .px(px(5.0))
                                .cursor_pointer()
                                .child(label)
                                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                    cx.stop_propagation();
                                    action_entity.update(cx, |this, cx| {
                                        if stage {
                                            this.git_action(
                                                vec![
                                                    "add".into(),
                                                    "--".into(),
                                                    action_path.clone().into_os_string(),
                                                ],
                                                cx,
                                            );
                                        } else {
                                            this.unstage(action_path.clone(), cx);
                                        }
                                    });
                                }),
                        );
                    }
                    row
                })
                .collect()
        })
        .flex_1()
        .min_h_0();
        let mut sidebar = div()
            .w(px(260.0))
            .min_w(px(180.0))
            .max_w(relative(0.45))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(tokens.border_subtle)
            .child(
                div()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(10.0))
                    .child(
                        div()
                            .id("browse-files")
                            .cursor_pointer()
                            .text_color(if self.git {
                                tokens.text_muted
                            } else {
                                tokens.text_primary
                            })
                            .child("Project")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.git = false;
                                    this.changed(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("browse-git")
                            .cursor_pointer()
                            .text_color(if self.git {
                                tokens.text_primary
                            } else {
                                tokens.text_muted
                            })
                            .child(format!(
                                "Git ({})",
                                self.repository.as_ref().map_or(0, |r| r.changes.len())
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.git = true;
                                    this.refresh(cx);
                                    this.changed(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("refresh-files")
                            .cursor_pointer()
                            .child("↻")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.refresh(cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(28.0))
                    .px(px(10.0))
                    .text_size(px(11.0))
                    .text_color(tokens.text_secondary)
                    .truncate()
                    .child(if self.git {
                        self.repository
                            .as_ref()
                            .map_or("No Git repository".into(), |r| r.branch.clone())
                    } else {
                        self.root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }),
            )
            .child(rows);
        if self.git && self.repository.is_some() {
            sidebar = sidebar.child(
                div()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(Input::new(&self.commit))
                    .child(
                        div()
                            .id("commit-files")
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .child(if self.operation.is_some() {
                                "Working…"
                            } else {
                                "Commit staged changes"
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    let message = this.commit.read(cx).value().to_string();
                                    if message.trim().is_empty() {
                                        this.message = "Enter a commit message first.".into();
                                        cx.notify();
                                        return;
                                    }
                                    this.git_action(
                                        vec!["commit".into(), "-m".into(), message.into()],
                                        cx,
                                    );
                                }),
                            ),
                    ),
            );
        }
        let mut tabs = div()
            .h(px(34.0))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .overflow_hidden()
            .border_b_1()
            .border_color(tokens.border_subtle);
        for (index, editor) in self.editors.iter().enumerate() {
            let dirty = editor.dirty;
            let path = editor.path.clone();
            let close_path = path.clone();
            let active = self.selected.as_ref() == Some(&path) && self.diff.is_none();
            tabs = tabs.child(
                div()
                    .id(("editor-tab", index))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(6.0))
                    .h_full()
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .when(active, |s| s.bg(tokens.card_hover))
                    .child(
                        svg()
                            .path(crate::file_icons::icon_for_path(&path.to_string_lossy()))
                            .size(px(15.0))
                            .text_color(tokens.text_secondary),
                    )
                    .child(format!(
                        "{}{}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        if dirty { " •" } else { "" }
                    ))
                    .child(div().id(("close-editor", index)).child("×").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if let Some(editor) = this.editors.iter().find(|e| e.path == close_path)
                                && (editor.saving || editor.dirty)
                            {
                                this.message = "Save your changes before closing this tab.".into();
                                cx.notify();
                                return;
                            }
                            this.editors.retain(|e| e.path != close_path);
                            if this.selected.as_ref() == Some(&close_path) {
                                this.selected = this.editors.last().map(|e| e.path.clone());
                            }
                            this.focus_selected(window, cx);
                            this.changed(cx);
                        }),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.selected = Some(path.clone());
                            this.diff = None;
                            this.focus_selected(window, cx);
                            this.changed(cx);
                        }),
                    ),
            );
        }
        let input = self.diff.clone().or_else(|| {
            self.editors
                .iter()
                .find(|e| Some(&e.path) == self.selected.as_ref())
                .map(|e| e.input.clone())
        });
        let body = if let Some(input) = input {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    Input::new(&input)
                        .appearance(false)
                        .bordered(false)
                        .font_family(crate::MONO_FONT)
                        .text_size(px(13.0))
                        .line_height(px(21.0))
                        .size_full(),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(tokens.text_muted)
                .child("Select a file to edit, or a Git change to review")
                .into_any_element()
        };
        let mut tools = div().flex().items_center().gap(px(10.0));
        if self.diff.is_some() {
            tools = tools.child(
                div()
                    .id("edit-diff-file")
                    .cursor_pointer()
                    .child("Edit working file")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            let path = this
                                .repository
                                .as_ref()
                                .zip(this.diff_path.as_ref())
                                .and_then(|(repo, path)| {
                                    repo.root
                                        .join(path)
                                        .strip_prefix(&this.root)
                                        .ok()
                                        .map(Path::to_path_buf)
                                });
                            if let Some(path) = path {
                                this.open(
                                    SavedFile {
                                        path,
                                        ..Default::default()
                                    },
                                    true,
                                    window,
                                    cx,
                                );
                            } else {
                                this.message = "This file is outside the project folder.".into();
                                cx.notify();
                            }
                        }),
                    ),
            );
        } else if self.selected.is_some() {
            tools = tools
                .child(
                    div()
                        .id("reload-file")
                        .cursor_pointer()
                        .child("Discard draft & reload")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.reload_selected(window, cx)),
                        ),
                )
                .child(
                    div()
                        .id("save-file")
                        .cursor_pointer()
                        .child("Save  Ctrl/Cmd+S")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.save_selected(cx)),
                        ),
                );
        }
        div()
            .debug_selector(|| "files-workspace".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().editor_background())
            .text_color(tokens.text_primary)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
                    && event.keystroke.key == "s"
                {
                    cx.stop_propagation();
                    this.save_selected(cx);
                }
            }))
            .child(
                div().flex_1().min_h_0().flex().child(sidebar).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(tabs)
                        .child(
                            div()
                                .h(px(28.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(11.0))
                                .text_color(tokens.text_secondary)
                                .child(
                                    if self.diff.is_some() {
                                        self.diff_path.as_ref()
                                    } else {
                                        self.selected.as_ref()
                                    }
                                    .map_or(String::new(), |p| p.to_string_lossy().into_owned()),
                                )
                                .child(tools),
                        )
                        .child(body),
                ),
            )
            .child(
                div()
                    .h(px(24.0))
                    .px(px(10.0))
                    .text_size(px(11.0))
                    .text_color(tokens.text_secondary)
                    .truncate()
                    .child(self.message.clone()),
            )
    }
}
impl FilesSurface {
    fn reload_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation.is_some() {
            return;
        }
        let Some(editor) = self
            .editors
            .iter()
            .find(|e| Some(&e.path) == self.selected.as_ref())
        else {
            return;
        };
        let path = editor.path.clone();
        let before = editor.input.read(cx).value();
        let root = self.root.clone();
        let handle = window.window_handle();
        self.operation = Some(cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { read_text(&root, &read_path) })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.operation = None;
                    if let Some(editor) = this.editors.iter_mut().find(|e| e.path == path) {
                        match result {
                            Ok(text) if editor.input.read(cx).value() == before => {
                                editor.baseline = text.clone();
                                editor.dirty = false;
                                editor
                                    .input
                                    .update(cx, |input, cx| input.set_value(text, window, cx));
                                this.message = "Reloaded from disk".into();
                            }
                            Ok(_) => {
                                this.message =
                                    "Draft changed while reloading; your new edits were kept."
                                        .into()
                            }
                            Err(error) => this.message = error,
                        }
                    }
                    this.changed(cx);
                });
            });
        }));
    }
    fn unstage(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.operation.is_some() {
            return;
        }
        let Some(repo) = &self.repository else {
            return;
        };
        let root = repo.root.clone();
        self.operation = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if run(&root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
                        git(
                            &root,
                            &[
                                "reset".as_ref(),
                                "-q".as_ref(),
                                "HEAD".as_ref(),
                                "--".as_ref(),
                                path.as_os_str(),
                            ],
                        )
                    } else {
                        git(
                            &root,
                            &[
                                "rm".as_ref(),
                                "--cached".as_ref(),
                                "--".as_ref(),
                                path.as_os_str(),
                            ],
                        )
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.operation = None;
                this.message = result.err().unwrap_or_default();
                this.refresh(cx);
                this.changed(cx);
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_sorts_directories_first_and_expands_only_requested_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let (rows, truncated) = tree(dir.path(), &BTreeSet::new()).unwrap();
        assert!(!truncated);
        assert_eq!(
            rows.iter().map(|e| e.path.as_path()).collect::<Vec<_>>(),
            [
                Path::new("src"),
                Path::new(".hidden"),
                Path::new("Cargo.toml")
            ]
        );
        let (rows, _) = tree(dir.path(), &BTreeSet::from([PathBuf::from("src")])).unwrap();
        assert_eq!(rows[1].path, Path::new("src/main.rs"));
        assert_eq!(rows[1].depth, 1);
        assert!(read_text(dir.path(), Path::new("src")).is_err());
        assert!(read_text(dir.path(), Path::new("../outside")).is_err());
    }

    #[test]
    fn saves_preserve_text_and_reject_external_changes_binary_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = Path::new("file.rs");
        let original = "// original\r\nfn main() {}\r\n";
        std::fs::write(dir.path().join(path), original).unwrap();
        let edited = "// edited\r\nfn main() {}\r\n";
        save(dir.path(), path, original, edited).unwrap();
        assert_eq!(read_text(dir.path(), path).unwrap(), edited);
        assert!(
            save(dir.path(), path, original, "overwrite")
                .unwrap_err()
                .contains("changed on disk")
        );
        assert_eq!(read_text(dir.path(), path).unwrap(), edited);
        std::fs::write(dir.path().join(path), [0, 1]).unwrap();
        assert!(read_text(dir.path(), path).unwrap_err().contains("Binary"));
        std::fs::File::create(dir.path().join(path))
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(read_text(dir.path(), path).unwrap_err().contains("1 MiB"));
    }

    #[test]
    fn git_status_and_operations_preserve_literal_paths_and_worktree_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]).unwrap();
        run(root, &["config", "user.name", "Test"]).unwrap();
        run(root, &["config", "user.email", "test@example.com"]).unwrap();
        let name = "[literal] file.txt";
        std::fs::write(root.join(name), "initial\n").unwrap();
        run(root, &["add", "--", name]).unwrap();
        let repo = repository(root).unwrap();
        assert!(repo.changes[0].staged);
        run(root, &["rm", "--cached", "--", name]).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(name)).unwrap(),
            "initial\n"
        );
        run(root, &["add", "--", name]).unwrap();
        run(root, &["commit", "-qm", "initial"]).unwrap();
        std::fs::write(root.join(name), "staged\n").unwrap();
        run(root, &["add", "--", name]).unwrap();
        std::fs::write(root.join(name), "working\n").unwrap();
        let repo = repository(root).unwrap();
        assert_eq!(repo.changes[0].path, Path::new(name));
        assert!(repo.changes[0].staged && repo.changes[0].unstaged);
        run(root, &["reset", "-q", "HEAD", "--", name]).unwrap();
        assert!(!repository(root).unwrap().changes[0].staged);
        assert_eq!(
            std::fs::read_to_string(root.join(name)).unwrap(),
            "working\n"
        );
        let renamed = parse_status("R  new name\0old name\0?? new\nfile\0").unwrap();
        assert_eq!(renamed.len(), 2);
        assert_eq!(renamed[1].path, Path::new("new\nfile"));
        assert!(parse_status("M  ../outside\0").is_err());
    }

    #[test]
    fn files_workspace_restores_drafts_and_renders_both_themes() {
        use crate::resize_tests::{DrawFrame as _, TestAppContext, open_app};
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("src")).unwrap();
        std::fs::write(fixture.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let root = fixture.path().to_path_buf();
        run(&root, &["init", "-q"]).unwrap();
        run(&root, &["config", "user.name", "Test"]).unwrap();
        run(&root, &["config", "user.email", "test@example.com"]).unwrap();
        run(&root, &["add", "."]).unwrap();
        run(&root, &["commit", "-qm", "initial"]).unwrap();
        cx.update_window_entity(&state, |state, window, cx| {
            state.projects[0].path = root.clone();
            state.settings.agent_layout = true;
            state.open_tool_panel(1, window, cx);
        });
        cx.draw_frame().unwrap();
        cx.update(|window, _| {
            window
                .render_to_image()
                .unwrap()
                .save("/tmp/shob-files-before.png")
                .unwrap()
        });
        cx.update_window_entity(&state, |state, window, cx| {
            let panel = state.focused_pane_id.unwrap();
            state.open_files_surface(panel, window, cx);
        });
        cx.run_until_parked();
        let view = state.read_with(&cx, |state, _| {
            state
                .pane_ref(state.focused_pane_id.unwrap())
                .unwrap()
                .files_surface
                .as_ref()
                .unwrap()
                .0
                .clone()
        });
        cx.update_window_entity(&view, |view, window, cx| {
            view.expanded.insert("src".into());
            view.refresh(cx);
            view.open(
                SavedFile {
                    path: "src/main.rs".into(),
                    ..Default::default()
                },
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        cx.update_window_entity(&view, |view, window, cx| {
            view.editors[0].input.update(cx, |input, cx| {
                input.set_value("fn main() { println!(\"draft\"); }\n", window, cx);
                let before = input.value();
                let oversized = "é".repeat(MAX_FILE_BYTES);
                use gpui::EntityInputHandler as _;
                input.replace_text_in_range(None, &oversized, window, cx);
                assert_eq!(input.value(), before);
                input.replace_and_mark_text_in_range(None, &oversized, None, window, cx);
                assert_eq!(input.value(), before);
            });
        });
        cx.run_until_parked();
        cx.update_window_entity(&view, |view, _, cx| assert!(!view.prepare_close(cx)));
        let saved = view.read_with(&cx, |view, cx| view.snapshot(cx));
        assert_eq!(saved.tabs[0].baseline.as_deref(), Some("fn main() {}\n"));
        assert!(saved.tabs[0].draft.as_ref().unwrap().contains("draft"));
        let saved: SavedFiles =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        cx.update_window_entity(&state, |state, window, cx| {
            let panel = state.focused_pane_id.unwrap();
            state.show_panel_terminal(panel, window, cx);
            state.pane_mut(panel).unwrap().files_surface = None;
            let files = state.build_files_surface(panel, root.clone(), saved, window, cx);
            state.pane_mut(panel).unwrap().files_surface = Some(files);
            state.pane_mut(panel).unwrap().files_active = true;
            state.pane_mut(panel).unwrap().display_title = "Files".into();
        });
        cx.run_until_parked();
        for (preference, name) in [
            (crate::ThemePreference::Dark, "dark"),
            (crate::ThemePreference::Light, "light"),
        ] {
            cx.update_window_entity(&state, |state, window, cx| {
                state.theme_preference = preference;
                state.apply_theme_preference(Some(window), cx);
            });
            cx.run_until_parked();
            cx.draw_frame().unwrap();
            assert!(cx.debug_bounds("files-workspace").is_some());
            cx.update(|window, _| {
                window
                    .render_to_image()
                    .unwrap()
                    .save(format!("/tmp/shob-files-after-{name}.png"))
                    .unwrap()
            });
        }
        state.read_with(&cx, |state, cx| {
            let pane = state.pane_ref(state.focused_pane_id.unwrap()).unwrap();
            let restored = pane.files_surface.as_ref().unwrap().0.read(cx);
            assert_eq!(
                restored.editors[0].input.read(cx).value(),
                "fn main() { println!(\"draft\"); }\n"
            );
            assert!(restored.expanded.contains(Path::new("src")));
            assert_eq!(
                std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
                "fn main() {}\n"
            );
        });
        cx.draw_frame().unwrap();
        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() { println!(\"draft\"); }\n"
        );
        let restored = state.read_with(&cx, |state, _| {
            state
                .pane_ref(state.focused_pane_id.unwrap())
                .unwrap()
                .files_surface
                .as_ref()
                .unwrap()
                .0
                .clone()
        });
        cx.update_window_entity(&restored, |view, _, cx| {
            view.git = true;
            view.refresh(cx);
        });
        cx.run_until_parked();
        cx.update_window_entity(&restored, |view, window, cx| {
            assert!(!view.editors[0].dirty);
            assert!(view.prepare_close(cx));
            let change = view.repository.as_ref().unwrap().changes[0].clone();
            view.show_diff(change, window, cx);
        });
        cx.run_until_parked();
        restored.read_with(&cx, |view, cx| {
            assert!(
                view.diff
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .value()
                    .contains("+fn main()")
            )
        });
        cx.draw_frame().unwrap();
        cx.update(|window, _| {
            window
                .render_to_image()
                .unwrap()
                .save("/tmp/shob-files-git.png")
                .unwrap()
        });
    }
}
