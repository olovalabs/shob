use anyhow::{Context, Result, bail};
use base64::Engine as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const SKILL: &str = r#"---
name: kathamo-status
description: Report agent turn status to the Kathamo terminal when available.
---

# Reporting status in Kathamo

When working inside a Kathamo terminal, report meaningful turn transitions so its sidebar can show accurate status. Run these commands from the same terminal:

- Start substantial work: `kathamo report working "what you are doing"`
- Ask the user or request approval: `kathamo report blocked "what you need" --kind question` (use `permission`, `question`, or `auth` as appropriate)
- Resume work: `kathamo report working "continuing"`
- Finish a turn: `kathamo report done "brief result"`
- A turn fails: `kathamo report error "brief reason"`
- Clear a stale report when abandoning a turn: `kathamo report clear`
- Report progress when useful: `kathamo report working "..." --progress 40`

Only report states that reflect your actual current state. Do not report `done` merely because the process is quiet or waiting. These reports are optional; if the command is unavailable, continue normally.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportArgs {
    pub state: String,
    pub message: Option<String>,
    pub kind: Option<String>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub id: Option<String>,
}

pub fn parse_report_args(args: &[String]) -> Result<Option<ReportArgs>> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(None);
    }
    let mut positional = Vec::new();
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut id = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg.starts_with("--") {
            i += 1;
            let value = args
                .get(i)
                .with_context(|| format!("missing value for {arg}"))?
                .clone();
            match arg.as_str() {
                "--kind" => kind = Some(value),
                "--progress" => {
                    progress = Some(
                        value
                            .parse::<u8>()
                            .context("progress must be an integer from 0 to 100")?,
                    )
                }
                "--app" => app = Some(value),
                "--id" => id = Some(value),
                _ => bail!("unknown option: {arg}"),
            }
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }
    let Some(state) = positional.first().cloned() else {
        bail!("usage: kathamo report <idle|working|done|blocked|error|clear> [message] [options]")
    };
    if !["idle", "working", "done", "blocked", "error", "clear"].contains(&state.as_str()) {
        bail!("unknown report state: {state}");
    }
    if positional.len() > 2 {
        bail!("expected one optional message");
    }
    if progress.is_some_and(|p| p > 100) {
        bail!("progress must be from 0 to 100");
    }
    for (label, value) in [
        ("kind", kind.as_deref()),
        ("app", app.as_deref()),
        ("id", id.as_deref()),
    ] {
        if value.is_some_and(|v| {
            v.is_empty() || v.chars().any(|c| c.is_control() || c == ':' || c == ';')
        }) {
            bail!("invalid {label} value");
        }
    }
    Ok(Some(ReportArgs {
        state,
        message: positional.get(1).cloned(),
        kind,
        progress,
        app,
        id,
    }))
}

pub fn encode_report(report: &ReportArgs) -> String {
    let mut fields = vec![format!("state={}", report.state)];
    if let Some(message) = &report.message {
        let encoded = base64::engine::general_purpose::STANDARD.encode(message.as_bytes());
        fields.push(format!("msg={encoded}"));
    }
    if let Some(value) = &report.kind {
        fields.push(format!("kind={value}"));
    }
    if let Some(value) = report.progress {
        fields.push(format!("progress={value}"));
    }
    if let Some(value) = &report.app {
        fields.push(format!("app={value}"));
    }
    if let Some(value) = &report.id {
        fields.push(format!("id={value}"));
    }
    format!("\x1b]7501;{}\x1b\\", fields.join(":"))
}

pub fn run_report(args: &[String]) -> Result<()> {
    match parse_report_args(args)? {
        Some(report) => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(encode_report(&report).as_bytes())?;
            stdout.flush()?;
        }
        None => println!(
            "Usage: kathamo report <state> [message] [--kind KIND] [--progress 0..100] [--app NAME] [--id ID]\nStates: idle, working, done, blocked, error, clear"
        ),
    }
    Ok(())
}

/// Send a versioned lifecycle record to the Kathamo instance that launched the
/// integration. Node adapters pass a JSON object on stdin and do not write to
/// the terminal screen; the app validates the pane generation and sequence.
#[cfg(unix)]
fn parse_status_event(
    input: &str,
    app_id: String,
    pane_id: u64,
    pane_generation: String,
) -> Result<crate::status_bus::StatusEvent> {
    let mut input: serde_json::Value =
        serde_json::from_str(input).context("invalid status event JSON")?;
    let record = input
        .as_object_mut()
        .context("status event must be a JSON object")?;
    // Adapters supply lifecycle data; only the launching terminal supplies the
    // route. Insert it before deserialization because adapters intentionally
    // omit these required wire-protocol fields.
    record.insert("app_id".into(), app_id.into());
    record.insert("pane_id".into(), pane_id.into());
    record.insert("pane_generation".into(), pane_generation.into());
    let mut event: crate::status_bus::StatusEvent =
        serde_json::from_value(input).context("invalid status event JSON")?;
    if event.reporter_generation.is_empty() {
        event.reporter_generation = event.session_id.clone().unwrap_or_else(|| "default".into());
    }
    Ok(event)
}

#[cfg(unix)]
pub fn run_status_event() -> Result<()> {
    use std::io::Read;
    let socket = std::env::var_os("KATHAMO_STATUS_SOCKET")
        .or_else(|| std::env::var_os("MARKUP_STATUS_SOCKET"))
        .context("not running in a Kathamo terminal")?;
    let mut input = String::new();
    std::io::stdin()
        .take(16 * 1024)
        .read_to_string(&mut input)?;
    let app_id = std::env::var("KATHAMO_APP_ID")
        .or_else(|_| std::env::var("MARKUP_APP_ID"))
        .context("missing Kathamo app identity")?;
    let pane_id = std::env::var("KATHAMO_PANE_ID")
        .or_else(|_| std::env::var("MARKUP_PANE_ID"))
        .context("missing Kathamo pane identity")?
        .parse()
        .context("invalid pane identity")?;
    let pane_generation = std::env::var("KATHAMO_PANE_GENERATION")
        .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))
        .context("missing Kathamo pane generation")?;
    let event = parse_status_event(&input, app_id, pane_id, pane_generation)?;
    crate::status_bus::send(Path::new(&socket), &event).context("unable to send status event")
}

#[cfg(unix)]
pub fn run_status(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("clear") if args.len() == 1 => {
            let app_id = std::env::var("KATHAMO_APP_ID")
                .or_else(|_| std::env::var("MARKUP_APP_ID"))
                .context("not running in a Kathamo terminal")?;
            let pane_id = std::env::var("KATHAMO_PANE_ID")
                .or_else(|_| std::env::var("MARKUP_PANE_ID"))?
                .parse()?;
            let pane_generation = std::env::var("KATHAMO_PANE_GENERATION")
                .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))?;
            let event = crate::status_bus::StatusEvent {
                version: 1,
                app_id,
                pane_id,
                pane_generation,
                reporter: "user".into(),
                reporter_generation: "user".into(),
                sequence: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
                    .min(u64::MAX as u128) as u64,
                session_id: None,
                turn_id: None,
                event: "clear".into(),
                message: None,
                approval_id: None,
                agent_id: None,
                parent: None,
            };
            let socket = std::env::var_os("KATHAMO_STATUS_SOCKET")
                .or_else(|| std::env::var_os("MARKUP_STATUS_SOCKET"))
                .context("status service is unavailable")?;
            crate::status_bus::send(Path::new(&socket), &event)
                .context("could not clear the pane status")
        }
        Some("explain") if args.len() == 2 && args[1] == "--json" => {
            let app_id = std::env::var("KATHAMO_APP_ID")
                .or_else(|_| std::env::var("MARKUP_APP_ID"))
                .context("not running in a Kathamo terminal")?;
            let pane_id: u64 = std::env::var("KATHAMO_PANE_ID")
                .or_else(|_| std::env::var("MARKUP_PANE_ID"))?
                .parse()?;
            let generation = std::env::var("KATHAMO_PANE_GENERATION")
                .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))?;
            let snapshot = std::env::var_os("KATHAMO_STATUS_SNAPSHOT")
                .or_else(|| std::env::var_os("MARKUP_STATUS_SNAPSHOT"))
                .map(PathBuf::from)
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| {
                    serde_json::from_slice::<crate::status_bus::StatusSnapshot>(&bytes).ok()
                });
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis();
            let valid = snapshot.as_ref().is_some_and(|s| {
                s.version == 1
                    && s.app_id == app_id
                    && s.pane_id == pane_id
                    && s.pane_generation == generation
                    && now.saturating_sub(s.updated_ms) <= 3000
            });
            if valid {
                println!("{}", serde_json::to_string(snapshot.as_ref().unwrap())?);
            } else {
                println!(
                    "{{\"version\":1,\"state\":\"unknown\",\"evidence\":\"snapshot-unavailable-or-stale\",\"confidence\":\"none\"}}"
                );
            }
            Ok(())
        }
        _ => bail!("usage: kathamo status <clear|explain --json>"),
    }
}

#[cfg(not(unix))]
pub fn run_status(_args: &[String]) -> Result<()> {
    bail!("native status commands are currently supported on Unix")
}

#[cfg(not(unix))]
pub fn run_status_event() -> Result<()> {
    bail!("native status events are currently supported on Unix")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTarget {
    Claude,
    Shared,
}

pub fn skill_paths(home: &Path, claude_config: Option<&Path>, target: SkillTarget) -> Vec<PathBuf> {
    match target {
        SkillTarget::Claude => vec![
            claude_config
                .unwrap_or(&home.join(".claude"))
                .join("skills/kathamo-status/SKILL.md"),
        ],
        SkillTarget::Shared => vec![home.join(".agents/skills/kathamo-status/SKILL.md")],
    }
}

fn targets(args: &[String]) -> Result<Vec<SkillTarget>> {
    if args.is_empty() {
        return Ok(vec![SkillTarget::Claude, SkillTarget::Shared]);
    }
    if args.len() != 2 || args[0] != "--agent" {
        bail!("usage: kathamo skill <setup|status|show|remove> [--agent claude|shared|all]");
    }
    let agent = args
        .windows(2)
        .find(|w| w[0] == "--agent")
        .map(|w| w[1].as_str());
    match agent {
        None | Some("all") => Ok(vec![SkillTarget::Claude, SkillTarget::Shared]),
        Some("claude") => Ok(vec![SkillTarget::Claude]),
        Some("shared") => Ok(vec![SkillTarget::Shared]),
        Some(other) => bail!("unknown agent target: {other} (use claude, shared, or all)"),
    }
}

pub fn run_skill(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("usage: kathamo skill <setup|status|show|remove> [--agent claude|shared|all]")
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let claude_config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let paths: Vec<PathBuf> = targets(&args[1..])?
        .into_iter()
        .flat_map(|t| skill_paths(&home, claude_config.as_deref(), t))
        .collect();
    match command {
        "show" => {
            print!("{SKILL}");
        }
        "setup" => {
            for path in &paths {
                std::fs::create_dir_all(path.parent().unwrap())?;
                if let Ok(existing) = std::fs::read_to_string(path) {
                    if existing != SKILL {
                        bail!(
                            "refusing to overwrite modified skill file {}",
                            path.display()
                        );
                    }
                    println!("Already installed {}", path.display());
                    continue;
                }
                let temp = path.with_extension(format!("kathamo-{}.tmp", std::process::id()));
                std::fs::write(&temp, SKILL)?;
                std::fs::rename(temp, path)?;
                println!("Installed {}", path.display());
            }
        }
        "status" => {
            for path in &paths {
                match std::fs::read_to_string(path) {
                    Ok(contents) if contents == SKILL => println!("Installed {}", path.display()),
                    Ok(_) => println!("Outdated {}", path.display()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        println!("Missing {}", path.display())
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        "remove" => {
            for path in &paths {
                match std::fs::read_to_string(path) {
                    Ok(contents) if contents == SKILL => {
                        std::fs::remove_file(path)?;
                        println!("Removed {}", path.display());
                    }
                    Ok(_) => eprintln!("Preserved modified skill {}", path.display()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        println!("Missing {}", path.display())
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        _ => bail!("unknown skill command: {command}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn adapter_lifecycle_payloads_receive_terminal_owned_route() {
        // This is the shape sent by the bundled Node adapters, which was
        // previously rejected before the terminal identity could be added.
        for reporter in ["opencode", "pi", "omp"] {
            for (sequence, state) in ["register", "working", "done"].into_iter().enumerate() {
                let payload = serde_json::json!({
                    "version": 1, "reporter": reporter,
                    "reporter_generation": "run-1", "sequence": sequence + 1,
                    "session_id": "session-1", "event": state
                });
                let event =
                    parse_status_event(&payload.to_string(), "app-1".into(), 17, "pane-run".into())
                        .unwrap();
                assert_eq!(event.app_id, "app-1");
                assert_eq!(event.pane_id, 17);
                assert_eq!(event.pane_generation, "pane-run");
                assert_eq!(event.event, state);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn adapter_cannot_choose_another_pane() {
        let payload = r#"{"version":1,"reporter":"pi","reporter_generation":"run-1","sequence":1,"event":"done","app_id":false,"pane_id":"wrong-type","pane_generation":null}"#;
        let event =
            parse_status_event(payload, "actual-app".into(), 8, "actual-pane".into()).unwrap();
        assert_eq!(event.app_id, "actual-app");
        assert_eq!(event.pane_id, 8);
        assert_eq!(event.pane_generation, "actual-pane");
        assert!(parse_status_event("[]", "app".into(), 8, "pane".into()).is_err());
    }

    #[test]
    fn report_encodes_message_and_options() {
        let args = vec![
            "working".into(),
            "hello: 🌍".into(),
            "--kind".into(),
            "task".into(),
            "--progress".into(),
            "42".into(),
            "--app".into(),
            "agent".into(),
            "--id".into(),
            "turn-1".into(),
        ];
        let report = parse_report_args(&args).unwrap().unwrap();
        assert_eq!(
            encode_report(&report),
            "\x1b]7501;state=working:msg=aGVsbG86IPCfjI0=:kind=task:progress=42:app=agent:id=turn-1\x1b\\"
        );
    }
    #[test]
    fn rejects_bad_state_and_progress() {
        assert!(parse_report_args(&["maybe".into()]).is_err());
        assert!(parse_report_args(&["working".into(), "--progress".into(), "101".into()]).is_err());
    }
    #[test]
    fn skills_only_target_owned_files() {
        let root = Path::new("/tmp/example");
        assert_eq!(
            skill_paths(root, None, SkillTarget::Shared),
            vec![root.join(".agents/skills/kathamo-status/SKILL.md")]
        );
    }
}
