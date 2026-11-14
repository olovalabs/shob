//! Optional native agent integrations. Hooks do nothing outside a Kathamo pane.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "Notification",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "Interrupt",
];

fn hook_command(agent: &str) -> String {
    format!(
        "if [ -n \"$KATHAMO_STATUS_SOCKET\" ] && [ -n \"$KATHAMO_EXECUTABLE\" ]; then \"$KATHAMO_EXECUTABLE\" status-hook {agent}; elif [ -n \"$MARKUP_STATUS_SOCKET\" ] && [ -n \"$MARKUP_EXECUTABLE\" ]; then \"$MARKUP_EXECUTABLE\" status-hook {agent}; fi"
    )
}

fn legacy_hook_command(agent: &str) -> String {
    format!(
        "if [ -n \"$KATHAMO_STATUS_TTY\" ] && [ -n \"$KATHAMO_EXECUTABLE\" ]; then \"$KATHAMO_EXECUTABLE\" status-hook {agent}; elif [ -n \"$MARKUP_STATUS_TTY\" ] && [ -n \"$MARKUP_EXECUTABLE\" ]; then \"$MARKUP_EXECUTABLE\" status-hook {agent}; fi"
    )
}

fn state_for_event(event: &Value) -> Option<&'static str> {
    // A worker (`agent_id`) reports to its pane too: the app lists it under the
    // parent and never lets it drive the parent's status or an alert.
    match event.get("hook_event_name").and_then(Value::as_str)? {
        "SessionStart" => Some("register"),
        "SessionEnd" => Some("clear"),
        "Interrupt" => Some("cancelled"),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Some("working"),
        "PermissionRequest" => Some("blocked"),
        "Notification" => match event.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt" | "elicitation_dialog") => Some("blocked"),
            _ => None,
        },
        "Stop" => Some("done"),
        _ => None,
    }
}

#[cfg(unix)]
pub fn run_hook(args: &[String]) -> Result<()> {
    let Some(agent) = args.first().filter(|a| *a == "claude" || *a == "codex") else {
        return Ok(());
    };
    let Some(socket) = std::env::var_os("KATHAMO_STATUS_SOCKET")
        .or_else(|| std::env::var_os("MARKUP_STATUS_SOCKET"))
    else {
        return Ok(());
    };
    let mut input = String::new();
    if std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)
        .is_err()
    {
        return Ok(());
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return Ok(());
    };
    let Some(state) = state_for_event(&event) else {
        return Ok(());
    };
    let _ = crate::status_bus::send(Path::new(&socket), &event_from_hook(agent, state, &event));
    Ok(())
}

#[cfg(unix)]
fn event_from_hook(agent: &str, state: &str, payload: &Value) -> crate::status_bus::StatusEvent {
    let session_id = payload
        .get("session_id")
        .or_else(|| payload.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let reporter_generation = session_id.clone().unwrap_or_else(|| {
        std::env::var("KATHAMO_PANE_GENERATION")
            .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))
            .unwrap_or_else(|_| "unknown-session".into())
    });
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    crate::status_bus::StatusEvent {
        version: 1,
        app_id: std::env::var("KATHAMO_APP_ID")
            .or_else(|_| std::env::var("MARKUP_APP_ID"))
            .unwrap_or_default(),
        pane_id: std::env::var("KATHAMO_PANE_ID")
            .or_else(|_| std::env::var("MARKUP_PANE_ID"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default(),
        pane_generation: std::env::var("KATHAMO_PANE_GENERATION")
            .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))
            .unwrap_or_default(),
        reporter: agent.to_owned(),
        reporter_generation,
        sequence,
        session_id,
        turn_id: payload
            .get("turn_id")
            .or_else(|| payload.get("turnId"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        event: state.to_owned(),
        message: None,
        approval_id: payload
            .get("tool_use_id")
            .or_else(|| payload.get("request_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent_id: payload
            .get("agent_id")
            .or_else(|| payload.get("agentId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        parent: None,
    }
}

#[cfg(not(unix))]
pub fn run_hook(_args: &[String]) -> Result<()> {
    Ok(())
}

fn hook_config(path: &Path, agent: &str, install: bool) -> Result<()> {
    let mut config: Value = match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .with_context(|| format!("invalid JSON in {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let root = config
        .as_object_mut()
        .context("agent settings must be a JSON object")?;
    if !install && !root.contains_key("hooks") {
        return Ok(());
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be a JSON object")?;
    let owned_command = hook_command(agent);
    let legacy_command = legacy_hook_command(agent);
    for event in EVENTS {
        if install
            && ((agent == "codex" && matches!(*event, "Notification" | "PostToolUseFailure"))
                || (agent == "claude"
                    && matches!(*event, "Interrupt" | "SessionEnd" | "PreToolUse")))
        {
            continue;
        }
        if !install && !hooks.contains_key(*event) {
            continue;
        }
        let groups = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("{event} hooks must be an array"))?;
        // Remove only our hook handlers, retaining all unrelated handlers in the
        // same group and all unrelated settings.
        groups.retain_mut(|group| {
            if let Some(items) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let previous_len = items.len();
                items.retain(|item| {
                    !matches!(item.get("command").and_then(Value::as_str), Some(command) if command == owned_command || command == legacy_command)
                });
                previous_len == items.len() || !items.is_empty()
            } else {
                true
            }
        });
        if install {
            groups.push(
                json!({"hooks":[{"type":"command", "command":hook_command(agent), "timeout":5}]}),
            );
        }
        if groups.is_empty() {
            hooks.remove(*event);
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    if !install && !path.exists() {
        return Ok(());
    }
    let parent = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("kathamo-{}.tmp", std::process::id()));
    let mut bytes = serde_json::to_vec_pretty(&config)?;
    bytes.push(b'\n');
    backup_before_replace(path)?;
    let write_result = (|| -> Result<()> {
        std::fs::write(&temp, bytes)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}

fn config_root(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}

fn detected_cli_version(binary: &str) -> Option<String> {
    let path = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(binary))
        .find(|path| path.is_file())?;
    let output = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.chars().take(180).collect())
    }
}

#[cfg(unix)]
fn print_live_status() {
    let snapshot = std::env::var_os("KATHAMO_STATUS_SNAPSHOT")
        .or_else(|| std::env::var_os("MARKUP_STATUS_SNAPSHOT"))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<crate::status_bus::StatusSnapshot>(&bytes).ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let app_id = std::env::var("KATHAMO_APP_ID")
        .or_else(|_| std::env::var("MARKUP_APP_ID"))
        .unwrap_or_default();
    let pane_gen = std::env::var("KATHAMO_PANE_GENERATION")
        .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))
        .unwrap_or_default();
    let pane_id = std::env::var("KATHAMO_PANE_ID")
        .or_else(|_| std::env::var("MARKUP_PANE_ID"))
        .ok()
        .and_then(|id| id.parse::<u64>().ok())
        .unwrap_or_default();
    let matches_env = snapshot.as_ref().is_some_and(|s| {
        s.app_id == app_id
            && s.pane_generation == pane_gen
            && s.pane_id == pane_id
            && now.saturating_sub(s.updated_ms) <= 3000
    });
    if matches_env {
        let s = snapshot.unwrap();
        if s.evidence == "agent-report" {
            println!(
                "Live reporting: Reporting from {} (state={}, confidence={})",
                s.reporter.as_deref().unwrap_or("agent"),
                s.state,
                s.confidence
            );
        } else {
            println!(
                "Live reporting: not observed yet (current pane state={}, evidence={})",
                s.state, s.evidence
            );
        }
    } else {
        println!(
            "Live reporting: Not verified; run this from a Kathamo terminal after an agent event."
        );
    }
}

#[cfg(not(unix))]
fn print_live_status() {
    println!("Live reporting: native socket transport is not available on this platform.");
}

fn backup_before_replace(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let backup = path.with_extension(format!("kathamo-{stamp}.bak"));
    if !backup.exists() {
        std::fs::copy(path, backup)?;
    }
    Ok(())
}

fn atomic_write_owned(path: &Path, contents: &str) -> Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing != contents
            && !existing.contains("Kathamo lifecycle adapter")
            && !existing.contains("Markup lifecycle adapter")
            && !existing.contains("kathamo-status")
            && !existing.contains("markup-status")
            && !existing.contains("HERDR_INTEGRATION_ID=opencode-tui")
        {
            bail!("refusing to overwrite existing file {}", path.display());
        }
    }
    let parent = path.parent().context("integration path has no parent")?;
    std::fs::create_dir_all(parent)?;
    backup_before_replace(path)?;
    let temp = path.with_extension(format!("kathamo-{}.tmp", std::process::id()));
    std::fs::write(&temp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn opencode_plugin_config(
    path: &Path,
    key: &str,
    spec: &str,
    install: Option<bool>,
) -> Result<bool> {
    use jsonc_parser::cst::{CstInputValue, CstRootNode};
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && install == Some(true) => {
            "{}\n".into()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let root = CstRootNode::parse(&content, &jsonc_parser::ParseOptions::default())
        .with_context(|| format!("invalid OpenCode config {}", path.display()))?;
    let object = root
        .value()
        .and_then(|v| v.as_object())
        .context("OpenCode config must be an object")?;
    let matches = |entry: &Value| {
        entry.as_str() == Some(spec)
            || entry.get("package").and_then(Value::as_str) == Some(spec)
            || entry
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                == Some(spec)
    };
    let property = object.get(key);
    let array = property
        .as_ref()
        .map(|p| p.array_value().context("OpenCode plugins must be an array"))
        .transpose()?;
    let found = array.as_ref().is_some_and(|a| {
        a.elements()
            .iter()
            .any(|e| e.to_serde_value().is_some_and(|e| matches(&e)))
    });
    let Some(install) = install else {
        return Ok(found);
    };
    if install == found {
        return Ok(found);
    }
    if install {
        if let Some(array) = array {
            array.append(CstInputValue::String(spec.into()));
        } else {
            object.append(
                key,
                CstInputValue::Array(vec![CstInputValue::String(spec.into())]),
            );
        }
    } else if let Some(array) = array {
        for entry in array.elements() {
            if entry.to_serde_value().is_some_and(|e| matches(&e)) {
                entry.remove();
            }
        }
        if array.elements().is_empty() {
            if let Some(property) = property {
                property.remove();
            }
        }
    }
    let parent = path.parent().context("OpenCode config has no parent")?;
    std::fs::create_dir_all(parent)?;
    backup_before_replace(path)?;
    let temp = path.with_extension(format!("kathamo-{}.tmp", std::process::id()));
    std::fs::write(&temp, root.to_string())?;
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temp, metadata.permissions())?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(temp);
        return Err(error.into());
    }
    Ok(install)
}

fn opencode_registration(home: &Path, mode: &str) -> Result<()> {
    let config = config_root("XDG_CONFIG_HOME", home.join(".config")).join("opencode");
    let state = config_root("XDG_STATE_HOME", home.join(".local/state")).join("opencode");
    let directory_spec = format!("file://{}", config.join("plugins/kathamo-status").display());
    let file_spec = format!("{directory_spec}/tui.js");
    let action = match mode {
        "setup" => Some(true),
        "remove" => Some(false),
        _ => None,
    };
    let mut configured = false;
    for path in [config.join("tui.jsonc"), config.join("tui.json")] {
        // Register in jsonc on setup; read/remove both formats without creating
        // tui.json, which OpenCode uses as a migration signal.
        let change = if mode == "setup" && path.ends_with("tui.json") {
            None
        } else {
            action
        };
        configured |= opencode_plugin_config(&path, "plugin", &file_spec, change)?;
    }
    let cli = config.join("cli.json");
    let migration_pending =
        !cli.exists() && (config.join("tui.json").is_file() || state.join("kv.json").is_file());
    if !migration_pending || mode != "setup" {
        configured |= opencode_plugin_config(&cli, "plugins", &directory_spec, action)?;
    } else {
        println!(
            "OpenCode: legacy TUI migration pending; restart OpenCode, then enable integrations again to register cli.json"
        );
    }
    if mode != "setup" && mode != "remove" {
        println!(
            "OpenCode plugin registration: {}",
            if configured {
                "registered; live reporting not verified"
            } else {
                "missing (plugin files alone do not enable reporting)"
            }
        );
    }
    Ok(())
}

fn update_rules(home: &Path) -> Result<()> {
    let config = config_root("XDG_CONFIG_HOME", home.join(".config"));
    let dir = config.join("kathamo/agent-detection-updates");
    let base = "https://raw.githubusercontent.com/herdrdev/herdr/master/src/detect/manifests";
    let ids: Vec<String> = crate::assets::AppAssets::iter()
        .filter(|p| {
            p.starts_with("agent-detection/") && p.ends_with(".toml") && !p.ends_with("index.toml")
        })
        .filter_map(|p| {
            p.rsplit('/')
                .next()?
                .strip_suffix(".toml")
                .map(str::to_owned)
        })
        .collect();
    let mut downloads = Vec::new();
    for id in ids {
        if !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        let url = format!("{base}/{id}.toml");
        let output = std::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "20",
                &url,
            ])
            .output()
            .with_context(|| "the explicit rules update requires curl")?;
        if !output.status.success() {
            bail!(
                "could not download agent detection manifest {id}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8(output.stdout).context("manifest is not UTF-8")?;
        let (manifest_id, version) =
            crate::agent_rules::validate_manifest(&text, &format!("{id}.toml"))
                .map_err(|error| anyhow::anyhow!("invalid upstream manifest {id}: {error}"))?;
        if manifest_id != id {
            bail!("upstream manifest name mismatch: requested {id}, received {manifest_id}");
        }
        downloads.push((id, version.unwrap_or_default(), text));
    }
    if downloads.is_empty() {
        bail!("no manifests were found to update");
    }
    std::fs::create_dir_all(&dir)?;
    let mut updated = 0;
    for (id, version, text) in downloads {
        let target = dir.join(format!("{id}.toml"));
        if let Ok(old) = std::fs::read_to_string(&target) {
            if let Ok((_, old_version)) =
                crate::agent_rules::validate_manifest(&old, &target.to_string_lossy())
            {
                if version_key(&version) <= version_key(old_version.as_deref().unwrap_or("")) {
                    continue;
                }
            }
        }
        if target.exists() {
            backup_before_replace(&target)?;
        }
        let temp = target.with_extension(format!("toml.{}.tmp", std::process::id()));
        std::fs::write(&temp, text)?;
        std::fs::rename(temp, target)?;
        updated += 1;
    }
    println!(
        "Updated {updated} validated agent detection manifests in {}. Restart Kathamo to load them.",
        dir.display()
    );
    Ok(())
}

fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn files(home: &Path) -> Vec<(PathBuf, &'static str)> {
    let pi = config_root("PI_CODING_AGENT_DIR", home.join(".pi/agent"));
    let omp = config_root(
        "OMP_AGENT_DIR",
        config_root("PI_CODING_AGENT_DIR", home.join(".omp/agent")),
    );
    let config = config_root("XDG_CONFIG_HOME", home.join(".config"));
    let opencode = config.join("opencode/plugins/kathamo-status");
    vec![
        (
            pi.join("extensions/kathamo-status.js"),
            include_str!("../assets/integrations/pi.js"),
        ),
        (
            omp.join("extensions/kathamo-status.js"),
            include_str!("../assets/integrations/omp.js"),
        ),
        (
            opencode.join("package.json"),
            include_str!("../assets/integrations/opencode-package.json"),
        ),
        (
            opencode.join("index.js"),
            include_str!("../assets/integrations/opencode-index.js"),
        ),
        (
            opencode.join("tui.js"),
            include_str!("../assets/integrations/opencode-tui.js"),
        ),
        (
            opencode.join("LICENSE"),
            include_str!("../assets/integrations/HERDR-LICENSE"),
        ),
        (
            opencode.join("NOTICE.md"),
            include_str!("../assets/integrations/NOTICE.md"),
        ),
    ]
}

pub fn run(args: &[String]) -> Result<()> {
    if args.len() != 1
        || !["setup", "status", "doctor", "remove", "update-rules"].contains(&args[0].as_str())
    {
        bail!("usage: kathamo integration <setup|status|doctor|remove|update-rules>");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let claude = config_root("CLAUDE_CONFIG_DIR", home.join(".claude")).join("settings.json");
    let codex = config_root("CODEX_HOME", home.join(".codex")).join("hooks.json");
    let mode = args[0].as_str();
    if mode == "update-rules" {
        return update_rules(&home);
    }
    let pi_root = config_root("PI_CODING_AGENT_DIR", home.join(".pi/agent"));
    let omp_root = config_root(
        "OMP_AGENT_DIR",
        config_root("PI_CODING_AGENT_DIR", home.join(".omp/agent")),
    );
    if mode == "setup" && pi_root == omp_root {
        bail!(
            "Pi and OMP resolve to the same extension directory ({}); set OMP_AGENT_DIR to a separate path before enabling both",
            pi_root.display()
        );
    }
    for (agent, path) in [("claude", claude), ("codex", codex)] {
        match mode {
            "setup" => {
                hook_config(&path, agent, true)?;
                println!(
                    "Installed {agent} hooks: {} (restart required{})",
                    path.display(),
                    if agent == "codex" {
                        "; Codex may require reviewing and trusting the hooks"
                    } else {
                        ""
                    }
                );
            }
            "remove" => {
                hook_config(&path, agent, false)?;
                println!("Removed {agent} hooks: {}", path.display());
            }
            _ => {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                println!(
                    "{agent}: {}",
                    if text.contains(&format!("status-hook {agent}")) {
                        if mode == "doctor" {
                            "installed; restart required; live reporting not verified"
                        } else if agent == "codex" {
                            "installed; hook trust and restart may be required; live reporting not verified"
                        } else {
                            "installed; restart required; live reporting not verified"
                        }
                    } else {
                        "missing"
                    }
                );
            }
        }
    }
    for (path, contents) in files(&home) {
        match mode {
            "setup" => {
                atomic_write_owned(&path, contents)?;
                println!(
                    "Installed {} (restart required; live reporting not verified)",
                    path.display()
                );
            }
            "remove" => match std::fs::read_to_string(&path) {
                Ok(existing) if existing == contents => {
                    std::fs::remove_file(&path)?;
                    println!("Removed {}", path.display());
                }
                Ok(_) => eprintln!("Preserved modified file {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
            _ => println!(
                "{}: {}",
                path.display(),
                match std::fs::read_to_string(&path) {
                    Ok(existing) if existing == contents =>
                        if mode == "doctor" {
                            "installed; restart required; live reporting not verified"
                        } else {
                            "installed; live reporting not verified"
                        },
                    Ok(_) => "modified; live reporting not verified",
                    Err(_) => "missing",
                }
            ),
        }
    }
    opencode_registration(&home, mode)?;
    // Retire only the exact adapter we previously installed; Command Code's
    // vendor reporter now uses the scoped Kathamo endpoint without a user mod.
    let legacy_kathamo = home.join(".commandcode/mods/kathamo-status.ts");
    let legacy_markup = home.join(".commandcode/mods/markup-status.ts");
    for legacy in [&legacy_kathamo, &legacy_markup] {
        if matches!(mode, "setup" | "remove")
            && std::fs::read_to_string(legacy).ok().as_deref()
                == Some(include_str!("../assets/integrations/commandcode.js"))
        {
            backup_before_replace(legacy)?;
            std::fs::remove_file(legacy)?;
        }
    }
    if mode != "remove" {
        println!(
            "Command Code: built-in native reporting available in new Kathamo panes; live reporting not verified"
        );
    }
    if mode == "doctor" {
        for (label, binary) in [
            ("Claude Code", "claude"),
            ("Codex", "codex"),
            ("OpenCode", "opencode"),
            ("Pi", "pi"),
            ("OMP", "omp"),
            ("Command Code", "cmd"),
        ] {
            println!(
                "{label} version: {}",
                detected_cli_version(binary).unwrap_or_else(|| "not detected".into())
            );
        }
        print_live_status();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opencode_registration_preserves_comments_and_unrelated_plugins() {
        let dir =
            std::env::temp_dir().join(format!("kathamo-opencode-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cli.json");
        let initial =
            "{\n // user comment\n \"theme\": \"dark\",\n \"plugins\": [\"another-plugin\",],\n}\n";
        std::fs::write(&path, initial).unwrap();
        let spec = "file:///tmp/kathamo-status";
        opencode_plugin_config(&path, "plugins", spec, Some(true)).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("// user comment"));
        assert!(first.contains("another-plugin"));
        assert!(first.contains("\"theme\": \"dark\""));
        assert!(opencode_plugin_config(&path, "plugins", spec, None).unwrap());
        opencode_plugin_config(&path, "plugins", spec, Some(true)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        opencode_plugin_config(&path, "plugins", spec, Some(false)).unwrap();
        let removed = std::fs::read_to_string(&path).unwrap();
        assert!(!removed.contains(spec));
        assert!(removed.contains("another-plugin") && removed.contains("// user comment"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn maps_lifecycle_including_subagents() {
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"UserPromptSubmit"})),
            Some("working")
        );
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"PermissionRequest"})),
            Some("blocked")
        );
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"Stop"})),
            Some("done")
        );
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"SessionStart", "session_id":"s1"})),
            Some("register")
        );
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"Interrupt", "session_id":"s1"})),
            Some("cancelled")
        );
        // A worker reports too; the app lists it under its parent.
        assert_eq!(
            state_for_event(&json!({"hook_event_name":"Stop", "agent_id":"child"})),
            Some("done")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_subagent_hook_carries_its_agent_id() {
        let event = event_from_hook(
            "claude",
            "working",
            &json!({"hook_event_name":"PreToolUse", "agent_id":"child"}),
        );
        assert_eq!(event.agent_id.as_deref(), Some("child"));
        assert!(event.parent.is_none());
    }
    #[test]
    fn preserves_existing_hooks_on_setup_and_remove() {
        let root = std::env::temp_dir().join(format!("kathamo-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        std::fs::write(&path, r#"{"other": 42, "hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing"}]}]}}"#).unwrap();
        hook_config(&path, "claude", true).unwrap();
        hook_config(&path, "claude", true).unwrap();
        let config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 2);
        hook_config(&path, "claude", false).unwrap();
        let config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["other"], 42);
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "existing"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
