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
    pub fn show_git(&mut self, cx: &mut Context<Self>) {
        self.git = true;
        self.refresh(cx);
        self.changed(cx);
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
    pub(super) fn focus_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
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

mod render;

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
mod tests;
