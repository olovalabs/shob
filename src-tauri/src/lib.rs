use base64::Engine;
use log::info;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const MAX_EDITOR_PREVIEW_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub color: Option<String>,
    #[serde(rename = "logoPath")]
    pub logo_path: Option<String>,
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub shell: String,
    #[serde(rename = "cliTool")]
    pub cli_tool: Option<String>,
    #[serde(rename = "pendingLaunchCommand")]
    pub pending_launch_command: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<u64>,
    #[serde(rename = "lastActiveAt")]
    pub last_active_at: Option<u64>,
    #[serde(rename = "commandCount")]
    pub command_count: Option<u64>,
    #[serde(rename = "startupDurationMs")]
    pub startup_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalHostInfo {
    pub os: String,
    #[serde(rename = "windowsBuildNumber")]
    pub windows_build_number: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProbeRequestItem {
    pub id: String,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProbeResult {
    pub id: String,
    pub installed: bool,
    #[serde(rename = "resolvedPath")]
    pub resolved_path: Option<String>,
    #[serde(rename = "matchedCommand")]
    pub matched_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTreeEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFileChange {
    pub path: String,
    #[serde(rename = "absolutePath")]
    pub absolute_path: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusSummary {
    #[serde(rename = "repoRoot")]
    pub repo_root: String,
    #[serde(rename = "changedFiles")]
    pub changed_files: Vec<GitFileChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFileState {
    #[serde(rename = "repoRoot")]
    pub repo_root: Option<String>,
    #[serde(rename = "baseContent")]
    pub base_content: String,
    #[serde(rename = "currentContent")]
    pub current_content: String,
    #[serde(rename = "hasChanges")]
    pub has_changes: bool,
    #[serde(rename = "isLargeFile")]
    pub is_large_file: bool,
    #[serde(rename = "fileSizeBytes")]
    pub file_size_bytes: u64,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBranchInfo {
    #[serde(rename = "repoName")]
    pub repo_name: Option<String>,
    pub head: String,
    #[serde(rename = "upstream")]
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBranchList {
    pub branches: Vec<String>,
}

struct ProjectWatcher {
    _root_path: String,
    _watcher: RecommendedWatcher,
}

static PROJECT_WATCHER: LazyLock<Mutex<Option<ProjectWatcher>>> =
    LazyLock::new(|| Mutex::new(None));

static LAST_WATCHER_OPERATION: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now() - Duration::from_secs(10)));

fn clear_project_watcher() {
    let mut manager = PROJECT_WATCHER.lock();
    if let Some(watcher) = manager.take() {
        drop(watcher);
    }
    *manager = None;
    std::thread::sleep(Duration::from_millis(100));
    *LAST_WATCHER_OPERATION.lock() = Instant::now();
}

fn should_forward_project_watch_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/").to_lowercase();

    if normalized.contains("/node_modules/")
        || normalized.contains("/dist/")
        || normalized.contains("/target/")
        || normalized.contains("/.next/")
        || normalized.contains("/.turbo/")
        || normalized.contains("/.cache/")
        || normalized.contains("/coverage/")
    {
        return false;
    }

    if normalized.contains("/.git/") {
        return normalized.ends_with("/.git/head")
            || normalized.ends_with("/.git/index")
            || normalized.ends_with("/.git/refs")
            || normalized.contains("/.git/refs/");
    }

    true
}

fn get_data_dir() -> PathBuf {
    let mut path = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("shob");
    std::fs::create_dir_all(&path).ok();
    path
}

fn get_projects_path() -> PathBuf {
    let mut path = get_data_dir();
    path.push("projects.json");
    path
}

fn get_sessions_dir() -> PathBuf {
    let mut path = get_data_dir();
    path.push("sessions");
    std::fs::create_dir_all(&path).ok();
    path
}

fn load_projects() -> Vec<Project> {
    let path = get_projects_path();
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

fn save_projects(projects: &[Project]) {
    let path = get_projects_path();
    if let Ok(content) = serde_json::to_string_pretty(projects) {
        std::fs::write(path, content).ok();
    }
}

fn get_image_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn detect_shells() -> Vec<String> {
    let mut shells = Vec::new();

    #[cfg(target_os = "windows")]
    {
        for command in ["powershell.exe", "cmd.exe"] {
            if let Some(resolved) = resolve_command(command) {
                shells.push(resolved);
            } else {
                shells.push(command.to_string());
            }
        }

        for git_shell in [
            "C:\\Program Files\\Git\\bin\\bash.exe",
            "C:\\Program Files\\Git\\bin\\sh.exe",
        ] {
            if std::path::Path::new(git_shell).exists() {
                shells.push(git_shell.to_string());
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        for shell in ["bash", "zsh", "fish", "sh"] {
            if Command::new("which")
                .arg(shell)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                shells.push(shell.to_string());
            }
        }
    }

    let mut unique_shells = Vec::new();
    for shell in shells {
        if !unique_shells.iter().any(|existing| existing == &shell) {
            unique_shells.push(shell);
        }
    }

    unique_shells
}

fn resolve_command(command: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let candidate = PathBuf::from(command);
        if candidate.is_absolute() && candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }

        let has_extension = candidate.extension().is_some();
        let pathext: Vec<String> = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
            .map(|ext| ext.to_ascii_lowercase())
            .collect();

        let path_entries = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();

        for dir in path_entries {
            let direct_candidate = dir.join(command);
            if direct_candidate.is_file() {
                return Some(direct_candidate.to_string_lossy().to_string());
            }

            if has_extension {
                continue;
            }

            for ext in &pathext {
                let ext = ext.trim_start_matches('.');
                let with_ext = dir.join(command).with_extension(ext);
                if with_ext.is_file() {
                    return Some(with_ext.to_string_lossy().to_string());
                }
            }
        }

        return None;
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(output) = Command::new("which").arg(command).output() {
            if output.status.success() {
                if let Some(found) = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|line| line.trim())
                    .find(|line| !line.is_empty())
                {
                    return Some(found.to_string());
                }
            }
        }

        None
    }
}

#[cfg(target_os = "windows")]
fn detect_windows_build_number() -> Option<u32> {
    let output = Command::new("cmd").args(["/C", "ver"]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let version_token = text
        .split_whitespace()
        .find(|token| token.chars().any(|char| char.is_ascii_digit()) && token.contains('.'))?;
    let cleaned = version_token.trim_matches(|char| char == '[' || char == ']');
    let build = cleaned
        .split('.')
        .nth(2)
        .and_then(|part| part.parse::<u32>().ok());

    build
}

#[cfg(not(target_os = "windows"))]
fn detect_windows_build_number() -> Option<u32> {
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectFsEvent {
    #[serde(rename = "projectPath")]
    project_path: String,
    paths: Vec<String>,
}

fn git_output(args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn count_file_lines(path: &Path) -> u32 {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().count() as u32,
        Err(_) => 0,
    }
}

fn read_text_file_if_exists_with_limit(path: &Path, max_bytes: u64) -> (String, u64, bool) {
    let file_size_bytes = std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if file_size_bytes > max_bytes {
        return (String::new(), file_size_bytes, true);
    }

    match std::fs::read_to_string(path) {
        Ok(content) => (content, file_size_bytes, false),
        Err(_) => (String::new(), file_size_bytes, false),
    }
}

fn read_git_file_if_exists_with_limit(
    repo_root_path: &Path,
    object_ref: &str,
    max_bytes: u64,
) -> (String, bool) {
    match Command::new("git")
        .args(["show", object_ref])
        .current_dir(repo_root_path)
        .output()
    {
        Ok(output) if output.status.success() => {
            if output.stdout.len() as u64 > max_bytes {
                (String::new(), true)
            } else {
                (String::from_utf8_lossy(&output.stdout).to_string(), false)
            }
        }
        _ => (String::new(), false),
    }
}

fn parse_repo_name_from_remote_url(remote_url: &str) -> Option<String> {
    let trimmed = remote_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .trim();

    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

fn to_git_relative_path(path: &Path, repo_root: &Path) -> Result<String, String> {
    path.strip_prefix(repo_root)
        .map_err(|_| "File is outside the repository".to_string())
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

fn list_directory_entries(path: &Path) -> Result<Vec<FileTreeEntry>, String> {
    let mut entries: Vec<FileTreeEntry> = std::fs::read_dir(path)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            let name = entry.file_name().to_string_lossy().to_string();

            Some(FileTreeEntry {
                name,
                path: entry.path().to_string_lossy().to_string(),
                is_directory: file_type.is_dir(),
            })
        })
        .collect();

    entries.sort_by(
        |left, right| match (left.is_directory, right.is_directory) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
        },
    );

    Ok(entries)
}

#[tauri::command]
fn get_projects() -> Vec<Project> {
    info!("Getting projects");
    load_projects()
}

#[tauri::command]
fn save_project(project: Project) -> Result<Project, String> {
    info!("Saving project: {}", project.name);
    let mut projects = load_projects();

    if let Some(idx) = projects.iter().position(|p| p.id == project.id) {
        projects[idx] = project.clone();
    } else {
        projects.push(project.clone());
    }

    save_projects(&projects);
    Ok(project)
}

#[tauri::command]
fn delete_project(project_id: String) -> Result<(), String> {
    info!("Deleting project: {}", project_id);
    let mut projects = load_projects();
    projects.retain(|p| p.id != project_id);
    save_projects(&projects);
    Ok(())
}

#[tauri::command]
fn save_session_output(session_id: String, output: String) -> Result<(), String> {
    let mut path = get_sessions_dir();
    path.push(format!("{}.log", session_id));
    std::fs::write(path, output).map_err(|e| e.to_string())
}

#[tauri::command]
fn load_session_output(session_id: String) -> Result<String, String> {
    let mut path = get_sessions_dir();
    path.push(format!("{}.log", session_id));
    if path.exists() {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    } else {
        Ok(String::new())
    }
}

#[tauri::command]
fn read_image_data_url(path: String) -> Result<String, String> {
    let image_path = PathBuf::from(&path);
    if !image_path.exists() || !image_path.is_file() {
        return Err("Image file not found".to_string());
    }

    let bytes = std::fs::read(&image_path).map_err(|err| err.to_string())?;
    let mime = get_image_mime_type(&image_path);
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);

    Ok(format!("data:{};base64,{}", mime, encoded))
}

#[tauri::command]
fn get_available_shells() -> Vec<String> {
    info!("Getting available shells");
    detect_shells()
}

#[tauri::command]
fn get_terminal_host_info() -> TerminalHostInfo {
    TerminalHostInfo {
        os: std::env::consts::OS.to_string(),
        windows_build_number: detect_windows_build_number(),
    }
}

#[tauri::command]
fn probe_cli_tools(items: Vec<CliProbeRequestItem>) -> Vec<CliProbeResult> {
    info!("Probing CLI tools");

    items
        .into_iter()
        .map(|item| {
            let mut resolved_path = None;
            let mut matched_command = None;

            for command in &item.commands {
                if let Some(path) = resolve_command(command) {
                    resolved_path = Some(path);
                    matched_command = Some(command.clone());
                    break;
                }
            }

            CliProbeResult {
                id: item.id,
                installed: resolved_path.is_some(),
                resolved_path,
                matched_command,
            }
        })
        .collect()
}

#[tauri::command]
fn set_project_watch(app: AppHandle, path: Option<String>) -> Result<(), String> {
    let mut manager = PROJECT_WATCHER.lock();

    if let Some(watcher) = manager.take() {
        drop(watcher);
    }

    let Some(path) = path else {
        info!("Cleared project watcher");
        *LAST_WATCHER_OPERATION.lock() = Instant::now();
        return Ok(());
    };

    let watch_root = PathBuf::from(&path);
    if !watch_root.exists() || !watch_root.is_dir() {
        return Err("Project watch path is invalid".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let last_op = *LAST_WATCHER_OPERATION.lock();
        let elapsed = last_op.elapsed();
        if elapsed < Duration::from_millis(200) {
            std::thread::sleep(Duration::from_millis(200) - elapsed);
        }
    }

    let project_path = watch_root.to_string_lossy().to_string();
    let app_handle = app.clone();
    let project_path_for_callback = project_path.clone();

    let mut watcher = RecommendedWatcher::new(
        move |result: Result<notify::Event, notify::Error>| {
            if let Ok(event) = result {
                let filtered_paths: Vec<String> = event
                    .paths
                    .iter()
                    .filter(|path| should_forward_project_watch_path(path))
                    .map(|path| path.to_string_lossy().to_string())
                    .collect();

                if filtered_paths.is_empty() {
                    return;
                }

                let payload = ProjectFsEvent {
                    project_path: project_path_for_callback.clone(),
                    paths: filtered_paths,
                };

                let _ = app_handle.emit("project-fs-event", payload);
            }
        },
        Config::default(),
    )
    .map_err(|e| format!("Failed to create file watcher: {}", e))?;

    watcher
        .watch(&watch_root, RecursiveMode::Recursive)
        .map_err(|e| format!("Failed to watch directory: {}", e))?;

    *manager = Some(ProjectWatcher {
        _root_path: project_path.clone(),
        _watcher: watcher,
    });

    *LAST_WATCHER_OPERATION.lock() = Instant::now();
    info!("Watching project path: {}", project_path);
    Ok(())
}

#[tauri::command]
fn list_directory(path: String) -> Result<Vec<FileTreeEntry>, String> {
    let directory = PathBuf::from(path);

    if !directory.exists() {
        return Err("Directory does not exist".to_string());
    }

    if !directory.is_dir() {
        return Err("Path is not a directory".to_string());
    }

    list_directory_entries(&directory)
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    let file_path = PathBuf::from(path);

    if !file_path.exists() {
        return Err("File does not exist".to_string());
    }

    if !file_path.is_file() {
        return Err("Path is not a file".to_string());
    }

    std::fs::read_to_string(file_path).map_err(|_| "Unable to read this file as text".to_string())
}

#[tauri::command]
fn get_git_status(path: String) -> Result<GitStatusSummary, String> {
    let cwd = PathBuf::from(path);
    let repo_root = git_output(&["rev-parse", "--show-toplevel"], &cwd)?
        .trim()
        .to_string();
    let repo_root_path = PathBuf::from(&repo_root);

    let status_output = git_output(&["status", "--porcelain"], &cwd)?;
    let numstat_output = git_output(&["diff", "--numstat", "HEAD"], &cwd).unwrap_or_default();

    let mut counts: HashMap<String, (u32, u32)> = HashMap::new();
    for line in numstat_output.lines() {
        let mut parts = line.split('\t');
        let additions = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        let deletions = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        let path = parts.next().unwrap_or("").to_string();
        if !path.is_empty() {
            counts.insert(path, (additions, deletions));
        }
    }

    let mut changed_files = Vec::new();
    for line in status_output.lines() {
        if line.len() < 4 {
            continue;
        }

        let status = line[..2].trim().to_string();
        let path = line[3..].trim().replace('\\', "/");
        let absolute_path = repo_root_path
            .join(&line[3..].trim().replace('/', std::path::MAIN_SEPARATOR_STR))
            .to_string_lossy()
            .to_string();
        let absolute_path_buf = PathBuf::from(&absolute_path);
        let (additions, deletions) = if status == "??" {
            (count_file_lines(&absolute_path_buf), 0)
        } else {
            counts.get(&path).copied().unwrap_or((0, 0))
        };

        changed_files.push(GitFileChange {
            path,
            absolute_path,
            status,
            additions,
            deletions,
        });
    }

    Ok(GitStatusSummary {
        repo_root,
        changed_files,
    })
}

#[tauri::command]
fn get_git_branch(path: String) -> Result<GitBranchInfo, String> {
    let cwd = PathBuf::from(path);
    let head = git_output(&["branch", "--show-current"], &cwd)?
        .trim()
        .to_string();

    let tracked_remote_name = if head.is_empty() {
        None
    } else {
        git_output(&["config", "--get", &format!("branch.{head}.remote")], &cwd)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    let upstream = git_output(
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        &cwd,
    )
    .ok()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());

    let remote_for_name = tracked_remote_name
        .clone()
        .or_else(|| {
            upstream
                .as_ref()
                .and_then(|value| value.split('/').next().map(str::to_string))
        })
        .or_else(|| {
            git_output(&["remote"], &cwd).ok().and_then(|value| {
                value
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string)
            })
        });

    let repo_name = remote_for_name.and_then(|remote_name| {
        git_output(&["remote", "get-url", &remote_name], &cwd)
            .ok()
            .and_then(|url| parse_repo_name_from_remote_url(&url))
    });

    Ok(GitBranchInfo {
        repo_name,
        head,
        upstream,
    })
}

#[tauri::command]
fn get_git_branches(path: String) -> Result<GitBranchList, String> {
    let cwd = PathBuf::from(path);
    let output = git_output(
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        &cwd,
    )?;

    let mut branches: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    branches.sort();

    Ok(GitBranchList { branches })
}

#[tauri::command]
fn switch_git_branch(path: String, branch: String) -> Result<(), String> {
    let cwd = PathBuf::from(path);

    let switch_attempt = Command::new("git")
        .args(["switch", &branch])
        .current_dir(&cwd)
        .output()
        .map_err(|e| e.to_string())?;

    if switch_attempt.status.success() {
        return Ok(());
    }

    let checkout_attempt = Command::new("git")
        .args(["checkout", &branch])
        .current_dir(&cwd)
        .output()
        .map_err(|e| e.to_string())?;

    if checkout_attempt.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&checkout_attempt.stderr)
            .trim()
            .to_string())
    }
}

#[tauri::command]
fn get_git_file_base(path: String) -> Result<String, String> {
    let file_path = PathBuf::from(path);
    let cwd = file_path
        .parent()
        .ok_or_else(|| "Unable to resolve file parent".to_string())?;

    let repo_root = git_output(&["rev-parse", "--show-toplevel"], cwd)?
        .trim()
        .to_string();
    let repo_root_path = PathBuf::from(&repo_root);
    let relative_path = to_git_relative_path(&file_path, &repo_root_path)?;
    let object_ref = format!("HEAD:{relative_path}");

    let output = Command::new("git")
        .args(["show", &object_ref])
        .current_dir(&repo_root_path)
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Ok(String::new())
    }
}

#[tauri::command]
fn get_git_file_state(path: String) -> Result<GitFileState, String> {
    let file_path = PathBuf::from(&path);
    let (current_content, file_size_bytes, current_is_large) =
        read_text_file_if_exists_with_limit(&file_path, MAX_EDITOR_PREVIEW_BYTES);

    let cwd = file_path
        .parent()
        .ok_or_else(|| "Unable to resolve file parent".to_string())?;

    let repo_root = match git_output(&["rev-parse", "--show-toplevel"], cwd) {
        Ok(value) => value.trim().to_string(),
        Err(_) => {
            return Ok(GitFileState {
                repo_root: None,
                base_content: String::new(),
                current_content,
                has_changes: false,
                is_large_file: current_is_large,
                file_size_bytes,
                status: String::new(),
                additions: 0,
                deletions: 0,
            })
        }
    };

    let repo_root_path = PathBuf::from(&repo_root);
    let relative_path = to_git_relative_path(&file_path, &repo_root_path)?;
    let status_output = git_output(
        &["status", "--porcelain", "--", &relative_path],
        &repo_root_path,
    )
    .unwrap_or_default();
    let status_line = status_output
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let status = if status_line.len() >= 2 {
        status_line[..2].trim().to_string()
    } else {
        String::new()
    };

    let numstat_output = git_output(
        &["diff", "--numstat", "HEAD", "--", &relative_path],
        &repo_root_path,
    )
    .unwrap_or_default();
    let mut additions = 0;
    let mut deletions = 0;
    if let Some(line) = numstat_output.lines().next() {
        let mut parts = line.split('\t');
        additions = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        deletions = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
    }

    if status == "??" {
        additions = current_content.lines().count() as u32;
        deletions = 0;
    }

    let object_ref = format!("HEAD:{relative_path}");
    let (base_content, base_is_large) =
        read_git_file_if_exists_with_limit(&repo_root_path, &object_ref, MAX_EDITOR_PREVIEW_BYTES);

    Ok(GitFileState {
        repo_root: Some(repo_root),
        base_content,
        current_content,
        has_changes: !status.is_empty(),
        is_large_file: current_is_large || base_is_large,
        file_size_bytes,
        status,
        additions,
        deletions,
    })
}

#[tauri::command]
fn cleanup_runtime() -> Result<(), String> {
    clear_project_watcher();
    info!("Cleaned up runtime state");
    Ok(())
}

#[tauri::command]
fn minimize_window(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    window.minimize().map_err(|err| err.to_string())
}

#[tauri::command]
fn toggle_maximize_window(app: AppHandle) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    let is_maximized = window.is_maximized().map_err(|err| err.to_string())?;

    if is_maximized {
        window.unmaximize().map_err(|err| err.to_string())?;
        Ok(false)
    } else {
        window.maximize().map_err(|err| err.to_string())?;
        Ok(true)
    }
}

#[tauri::command]
fn is_window_maximized(app: AppHandle) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    window.is_maximized().map_err(|err| err.to_string())
}

#[tauri::command]
fn close_window(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    window.close().map_err(|err| err.to_string())
}

#[tauri::command]
fn reveal_in_finder(path: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    if !target.exists() {
        return Err("Path does not exist".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let result = if target.is_dir() {
            Command::new("explorer")
                .raw_arg(format!("/select,\"{}\"", target.to_string_lossy()))
                .creation_flags(0x00000010)
                .spawn()
        } else if target.parent().is_some() {
            Command::new("explorer")
                .raw_arg(format!("/select,\"{}\"", target.to_string_lossy()))
                .creation_flags(0x00000010)
                .spawn()
        } else {
            Command::new("explorer")
                .raw_arg(format!("/root,\"{}\"", target.to_string_lossy()))
                .creation_flags(0x00000010)
                .spawn()
        };

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Failed to open file explorer: {}", e)),
        }
    }

    #[cfg(target_os = "macos")]
    {
        let result = if target.is_dir() {
            Command::new("open").arg(&target).spawn()
        } else {
            Command::new("open")
                .args(["-R", &target.to_string_lossy()])
                .spawn()
        };

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Failed to open Finder: {}", e)),
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let result = Command::new("xdg-open")
            .arg(target.parent().unwrap_or(&target))
            .spawn();

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Failed to open file manager: {}", e)),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!("Starting shob");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_pty::init())

        .invoke_handler(tauri::generate_handler![
            get_projects,
            save_project,
            delete_project,
            save_session_output,
            load_session_output,
            read_image_data_url,
            get_available_shells,
            get_terminal_host_info,
            probe_cli_tools,
            set_project_watch,
            list_directory,
            read_text_file,
            get_git_branch,
            get_git_branches,
            get_git_status,
            get_git_file_base,
            get_git_file_state,
            switch_git_branch,
            cleanup_runtime,
            minimize_window,
            toggle_maximize_window,
            is_window_maximized,
            close_window,
            reveal_in_finder,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, _event| {});
}
