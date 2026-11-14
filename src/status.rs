//! Live terminal status detection.
//!
//! Detection runs in two tiers, in this order:
//!
//! 1. **Agent rule packs.** If an agent CLI owns the shell's foreground process
//!    group, its herdr rule pack (`src/agent_rules.rs`) decides the state from
//!    the screen, the OSC title, and the OSC progress field. This is where
//!    `working` and `blocked` come from, and it is the only tier that
//!    understands an agent's own UI.
//! 2. **Conservative fallback.** Recent output or an animating busy marker can
//!    mean `Working`; an ambiguous live process remains `Unknown`.
//!
//! The foreground group is read from `/proc/<pid>/stat` field 8 (`tpgid`). Note
//! that a `portable-pty` shell is its own process-group leader, so `pgrp == pid`
//! while `tpgid` is the job actually running — reading the wrong field here once
//! made the whole process half of detection silently inert.
//!
//! herdr has no `error` state, so `Error` is ours: it comes from the terminal's
//! real exit code (the host applies that in `poll_sessions_status`) and, for
//! plain shells, from failure text on screen. Deliberately *not* applied while
//! an agent pack is active, because agent transcripts are full of `error:` lines
//! belonging to the code being discussed rather than to the terminal.
//!
//! Completion persistence is applied by the app lifecycle reducer, not the
//! screen detector: Done/Error survive focus and idle transitions, then clear
//! on new work or an explicit acknowledgment.
//!
//! Platform note: the process-tree half is Linux-only (`/proc`). Elsewhere
//! [`foreground_process_names`] returns nothing and detection rests on the
//! screen and OSC rules alone.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use crate::agent_rules::{self, DetectionInput, RuleState};
use regex::{RegexSet, RegexSetBuilder};

/// Shared empty snapshot, so idle panes do not allocate a new `Arc` every poll.
static EMPTY_FOREGROUND_NAMES: LazyLock<Arc<[String]>> =
    LazyLock::new(|| Arc::from(Vec::<String>::new()));
static UNKNOWN_FOREGROUND_NAMES: LazyLock<Arc<[String]>> =
    LazyLock::new(|| Arc::from(["<process-state-unavailable>".to_string()]));

/// The shared empty snapshot, for panes that have no pid or no foreground job.
pub fn empty_foreground_names() -> Arc<[String]> {
    Arc::clone(&*EMPTY_FOREGROUND_NAMES)
}

/// Snapshot used when the process state could not be inspected.
pub fn unknown_foreground_names() -> Arc<[String]> {
    Arc::clone(&*UNKNOWN_FOREGROUND_NAMES)
}

/// Foreground-group cache: shell pid -> (tpgid, shared process-name snapshot).
///
/// The poller runs every 500 ms per session, but the foreground group only
/// changes when a command starts or ends. The `stat` read still happens each
/// tick (one small read, and it is the change detector), while the heavier
/// `comm` / `cmdline` / `children` reads are skipped while the group is
/// unchanged. Snapshots are shared across the status and logo paths; closed
/// panes explicitly remove their entry.
#[cfg(target_os = "linux")]
static FG_CACHE: LazyLock<
    parking_lot::Mutex<HashMap<u32, (i64, Vec<(u32, Option<u64>)>, Arc<[String]>)>>,
> = LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// The status a terminal session can report. Exactly one is active at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// User is currently focused/typing in this terminal.
    Active,
    /// Not focused, nothing is running.
    Idle,
    /// A command or AI agent CLI is running.
    Working,
    /// Waiting for user input / approval / stuck.
    Blocked,
    /// Process failed (non-zero exit, crash, error output).
    Error,
    /// Task finished successfully.
    Done,
    /// Evidence is insufficient to tell whether an unfamiliar program is
    /// working, waiting, or finished while it remains open.
    Unknown,
}

impl SessionStatus {
    /// Render priority used when multiple panes contribute to a session.
    /// Herdr's order: blocked > done > working > idle. `Error` counts as
    /// blocked, `Unknown`/`Active` count as idle.
    pub fn rank(self) -> u8 {
        match self {
            SessionStatus::Error => 7,
            SessionStatus::Blocked => 6,
            SessionStatus::Done => 5,
            SessionStatus::Working => 4,
            SessionStatus::Unknown => 3,
            SessionStatus::Active => 2,
            SessionStatus::Idle => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Idle => "idle",
            SessionStatus::Working => "working",
            SessionStatus::Blocked => "blocked",
            SessionStatus::Error => "error",
            SessionStatus::Done => "done",
            SessionStatus::Unknown => "unknown",
        }
    }

    /// (dot, text, badge background) colors, from the spec.
    pub fn colors(self) -> (u32, u32, u32) {
        match self {
            SessionStatus::Active => (0x58a6ffff, 0x58a6ffff, 0x1f6feb33),
            SessionStatus::Idle => (0x8b949eff, 0x8b949eff, 0x21262d88),
            SessionStatus::Working => (0xd29922ff, 0xd29922ff, 0x9e6a0344),
            SessionStatus::Blocked => (0xdb6d28ff, 0xdb6d28ff, 0xbd561444),
            SessionStatus::Error => (0xf85149ff, 0xf85149ff, 0xda363344),
            SessionStatus::Done => (0x3fb950ff, 0x3fb950ff, 0x23863644),
            SessionStatus::Unknown => (0x9b9ba5ff, 0x74747eff, 0x77778122),
        }
    }
}

/// Provenance for a pane's current status reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    ProcessExit,
    AgentReport,
    AgentRule,
    AgentLifecycle,
    TerminalProgress,
    ScreenHeuristic,
    ForegroundProcess,
    NoSignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectionResult {
    pub status: SessionStatus,
    pub evidence: EvidenceSource,
    pub confidence: Confidence,
}

impl DetectionResult {
    pub fn new(status: SessionStatus, evidence: EvidenceSource, confidence: Confidence) -> Self {
        Self {
            status,
            evidence,
            confidence,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Idle,
    AwaitingApproval,
    Exited,
}

impl State {
    pub fn to_session_status(self) -> SessionStatus {
        match self {
            State::Running => SessionStatus::Working,
            State::Idle => SessionStatus::Idle,
            State::AwaitingApproval => SessionStatus::Blocked,
            State::Exited => SessionStatus::Done,
        }
    }
}

/// Patterns that describe one agent's UI. `generic()` works for most agents;
/// add per-agent patterns with `with_approval` / `with_running`.
#[derive(Debug, Clone)]
pub struct Profile {
    approval: Vec<String>,
    running: Vec<String>,
}

impl Profile {
    pub fn generic() -> Self {
        Self {
            approval: [
                r"\(y/n\)",
                r"\[y/n(/\w+)?\]",
                r"\by/n\b",
                r"do you want to (proceed|allow|run|make|create|apply|continue|edit)",
                r"\b(allow|approve|permit|grant)\b.*\?",
                r"press enter to (confirm|continue|approve)",
                r"[❯>›▶]\s*1[.)]\s*(yes|allow|approve|accept)",
                r"\b(esc|escape) to (cancel|deny|reject)",
                r"password:",
                r"yes \(y\)",
            ]
            .map(String::from)
            .to_vec(),
            running: [
                r"(esc|ctrl\+c|ctrl-c) to (interrupt|cancel|stop)",
                r"[\u{2800}-\u{28ff}\u{25d0}-\u{25d3}]",
                r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]", // braille spinners
                r"(thinking|working|generating|processing)\s*(…|\.\.\.)",
            ]
            .map(String::from)
            .to_vec(),
        }
    }

    pub fn with_approval(mut self, re: &str) -> Self {
        self.approval.push(re.into());
        self
    }

    pub fn with_running(mut self, re: &str) -> Self {
        self.running.push(re.into());
        self
    }

    pub fn compile(&self) -> (RegexSet, RegexSet) {
        let b = |v: &[String]| {
            RegexSetBuilder::new(v)
                .case_insensitive(true)
                .build()
                .expect("bad pattern")
        };
        (b(&self.approval), b(&self.running))
    }
}

/// Pure state machine: feed it screen text, get a state. It does not care
/// where the text came from (PTY, tmux capture-pane, ...), so it is easy to test.
pub struct Detector {
    approval: RegexSet,
    running: RegexSet,
    last_hash: u64,
    last_change: Instant,
    last_busy_hash: Option<u64>,
    last_busy_change: Instant,
    /// screen must be unchanged this long before we trust an approval prompt
    settle: Duration,
    /// screen must be unchanged this long (with no "running" marker) to be idle
    idle_after: Duration,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new(Profile::generic(), Instant::now())
    }
}

impl Detector {
    pub fn new(profile: Profile, now: Instant) -> Self {
        let (approval, running) = profile.compile();
        Self {
            approval,
            running,
            last_hash: 0,
            last_change: now,
            last_busy_hash: None,
            last_busy_change: now,
            settle: Duration::from_millis(400),
            idle_after: Duration::from_millis(1500),
        }
    }

    pub fn update(&mut self, screen: &str, exited: bool, now: Instant) -> State {
        if exited {
            return State::Exited;
        }
        let mut h = DefaultHasher::new();
        screen.hash(&mut h);
        let hash = h.finish();
        if hash != self.last_hash {
            self.last_hash = hash;
            self.last_change = now;
        }
        let stable = now.duration_since(self.last_change);
        let tail = tail_lines(screen, 12);

        // 1. a prompt waiting on the human (only once the screen stopped moving)
        if stable >= self.settle && self.approval.is_match(&tail) {
            return State::AwaitingApproval;
        }
        // Generic transcript text is weak evidence. Only changes to the busy
        // lines refresh it; an unrelated clock/cursor must not keep an old
        // spinner alive. Known agent rules and native reports remain authoritative.
        if self.running.is_match(&tail) {
            let mut h = DefaultHasher::new();
            for line in tail.lines().filter(|line| self.running.is_match(line)) {
                line.hash(&mut h);
            }
            let busy_hash = h.finish();
            if self.last_busy_hash != Some(busy_hash) {
                self.last_busy_hash = Some(busy_hash);
                self.last_busy_change = now;
            }
            return if now.duration_since(self.last_busy_change) < Duration::from_secs(5) {
                State::Running
            } else {
                // Idle here means no current evidence, never task completion.
                State::Idle
            };
        }
        self.last_busy_hash = None;
        // 3. screen still changing => busy; stable => idle at its prompt
        if stable < self.idle_after {
            State::Running
        } else {
            State::Idle
        }
    }

    pub fn settle(&self) -> Duration {
        self.settle
    }

    pub fn idle_after(&self) -> Duration {
        self.idle_after
    }
}

/// Last `n` non-empty lines of the screen, trailing whitespace trimmed.
///
/// Walks backwards and stops after `n` lines so a poller evaluating a quiet
/// screen does not collect and then discard every line above the window it
/// actually reads.
pub fn tail_lines(screen: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut tail: Vec<&str> = Vec::with_capacity(n);
    for line in screen.lines().rev().map(str::trim_end) {
        if line.is_empty() {
            continue;
        }
        tail.push(line);
        if tail.len() == n {
            break;
        }
    }
    tail.reverse();
    tail.join("\n")
}

/// All inputs needed for one detection pass.
pub struct StatusInput<'a> {
    /// Visible screen rows, top to bottom.
    pub lines: &'a [String],
    /// OSC 0/2 title, if the application set one.
    pub title: Option<&'a str>,
    /// Names in the shell's foreground process group. Empty at a prompt.
    pub foreground: &'a [String],
    /// State from the temporal detector, if available.
    pub detector_state: Option<State>,
    /// OSC 9;4 progress from the terminal, when the program reports it.
    pub osc_progress: &'a str,
}

const EXTRA_AGENTS: &[&str] = &[
    "aider",
    "goose",
    "cursor-agent",
    "plandex",
    "swept",
    "openman",
    "avante",
];

/// Does this process name belong to an agent CLI we recognize?
pub fn is_agent_process(name: &str) -> bool {
    agent_rules::identify(name).is_some()
        || EXTRA_AGENTS.iter().any(|&extra| {
            let key = name
                .rsplit('/')
                .next()
                .unwrap_or(name)
                .trim()
                .to_ascii_lowercase();
            key == extra || key.strip_suffix("-cli").is_some_and(|s| s == extra)
        })
}

/// The rule pack that owns any of `process_names`, for logging.
pub fn pack_for(process_names: &[String]) -> Option<&'static str> {
    agent_rules::embedded()
        .pack_for(process_names)
        .map(|pack| pack.id.as_str())
}

/// Which pack and rule produced the verdict, for `T3_STATUS_DEBUG` output.
///
/// `None` means either that no pack applies (a plain shell) or that the pack
/// matched no rule; the caller logs the pack separately to tell those apart.
pub fn explain(input: &StatusInput<'_>) -> Option<agent_rules::RuleMatch> {
    let screen = input.lines.join("\n");
    let pack = agent_rules::embedded().pack_for(input.foreground)?;
    agent_rules::evaluate(
        pack,
        DetectionInput {
            screen: &screen,
            osc_title: input.title.unwrap_or(""),
            osc_progress: input.osc_progress,
        },
    )
}

/// Candidate names for the process in the shell's foreground process group.
///
/// Three sources, because no single one is enough:
/// * `/proc/<pid>/comm` — set from the executable name (catches symlinked CLIs),
/// * `argv[0]` basename — reflects `exec -a` renames,
/// * `argv[1]` basename — where npm-installed agents live (`node …/claude`).
///
/// Empty when the shell itself owns the foreground group, i.e. the user is back
/// at a prompt, or on platforms without `/proc` (macOS and Windows rest on the
/// screen and OSC rules alone).
pub fn foreground_process_names(pid: Option<u32>) -> Arc<[String]> {
    let started = crate::diagnostics::foreground_start();
    let names = foreground_process_names_inner(pid);
    crate::diagnostics::foreground_end(started);
    names
}

#[cfg(target_os = "linux")]
fn foreground_process_names_inner(pid: Option<u32>) -> Arc<[String]> {
    let Some(pid) = pid else {
        return unknown_foreground_names();
    };
    let Some(tpgid) = read_proc_tpgid(pid) else {
        FG_CACHE.lock().remove(&pid);
        return unknown_foreground_names();
    };
    // `tpgid == 0` means no controlling terminal; `tpgid == pid` means the
    // shell itself owns the foreground group, i.e. it is sitting at a prompt.
    if tpgid == 0 {
        FG_CACHE.lock().remove(&pid);
        return unknown_foreground_names();
    }
    if tpgid == pid as i64 {
        FG_CACHE.lock().remove(&pid);
        return empty_foreground_names();
    }

    // Refresh process details every tick. A wrapper can launch or replace a
    // child without changing its process group, so a tpgid-only cache goes
    // stale. Reuse the old allocation only when the refreshed names match.
    let dir = Path::new("/proc").join(tpgid.to_string());
    let mut names = Vec::with_capacity(3);

    let mut push = |name: String| {
        let name = name.trim().to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    };

    fn read_proc_info(proc_dir: &Path, push: &mut impl FnMut(String)) {
        if let Ok(comm) = std::fs::read_to_string(proc_dir.join("comm")) {
            push(comm);
        }

        if let Ok(cmdline) = std::fs::read(proc_dir.join("cmdline")) {
            fn basename(arg: &[u8]) -> String {
                String::from_utf8_lossy(arg)
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
            let args: Vec<&[u8]> = cmdline
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .collect();
            if let Some(argv0) = args.first() {
                push(basename(argv0));
            }
            if let Some(argv0) = args.first() {
                const INTERPRETERS: &[&str] = &[
                    "node", "nodejs", "python", "python3", "deno", "bun", "ruby", "perl", "sh",
                    "bash", "npx", "bunx", "pnpm", "yarn", "sudo",
                ];
                let interpreter = basename(argv0).to_ascii_lowercase();
                if INTERPRETERS.contains(&interpreter.as_str()) {
                    // Find first non-flag argument
                    for arg in &args[1..] {
                        if !arg.starts_with(b"-") {
                            push(basename(arg));
                            break;
                        }
                    }
                }
            }
        }
    }

    read_proc_info(&dir, &mut push);

    // Walk descendants so nested shell and package-manager wrappers are
    // visible. Bound the walk to avoid pathological process trees.
    let mut pending = vec![tpgid as u32];
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent) || seen.len() > 256 {
            continue;
        }
        let task_dir = Path::new("/proc")
            .join(parent.to_string())
            .join("task")
            .join(parent.to_string());
        if let Ok(children) = std::fs::read_to_string(task_dir.join("children")) {
            for child in children
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
            {
                let child_dir = Path::new("/proc").join(child.to_string());
                read_proc_info(&child_dir, &mut push);
                pending.push(child);
            }
        }
    }

    if names.is_empty() {
        names.push("<process-state-unavailable>".to_string());
    }
    names.sort();
    let mut process_ids: Vec<(u32, Option<u64>)> = seen
        .into_iter()
        .map(|process_id| (process_id, read_proc_start_time(process_id)))
        .collect();
    process_ids.sort_unstable_by_key(|(process_id, _)| *process_id);
    cache_foreground_names(&mut FG_CACHE.lock(), pid, tpgid, process_ids, names)
}

#[cfg(target_os = "linux")]
fn cache_foreground_names(
    cache: &mut HashMap<u32, (i64, Vec<(u32, Option<u64>)>, Arc<[String]>)>,
    pid: u32,
    tpgid: i64,
    process_ids: Vec<(u32, Option<u64>)>,
    names: Vec<String>,
) -> Arc<[String]> {
    if let Some(previous) = cache
        .get(&pid)
        .filter(|(old_group, old_pids, old_names)| {
            *old_group == tpgid
                && *old_pids == process_ids
                && old_names.as_ref() == names.as_slice()
        })
        .map(|(_, _, old_names)| Arc::clone(old_names))
    {
        return previous;
    }
    let names: Arc<[String]> = names.into();
    cache.insert(pid, (tpgid, process_ids, Arc::clone(&names)));
    names
}

#[cfg(not(target_os = "linux"))]
fn foreground_process_names_inner(_pid: Option<u32>) -> Arc<[String]> {
    unknown_foreground_names()
}

/// Forget cached process names when a pane's shell is closed. The cache is
/// keyed by shell PID, so explicit removal prevents terminal churn from
/// retaining one entry per historical session until process shutdown.
pub fn forget_pid(pid: Option<u32>) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = pid {
        FG_CACHE.lock().remove(&pid);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

/// The `tpgid` field of `/proc/<pid>/stat`: the terminal's foreground process
/// group, i.e. whoever is *running right now* under this shell.
///
/// Parsing needs care. The fields are space-separated **after** a
/// parenthesised `comm`, which may itself contain spaces and parens, so the
/// only safe split is on the **last** `)`. Counting from there the fields are
/// 1-indexed as:
///
/// ```text
/// 3 state | 4 ppid | 5 pgrp | 6 session | 7 tty_nr | 8 tpgid
/// ```
///
/// Reading field 5 here was a real bug — that is `pgrp`, and a `portable-pty`
/// shell is its own process-group leader, so `pgrp == pid` and the guard below
/// rejected every result. The whole process-tree half of detection was silently
/// dead: a running CLI could never be reported as `Working`.
#[cfg(target_os = "linux")]
pub fn read_proc_tpgid(pid: u32) -> Option<i64> {
    let stat =
        std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    parse_stat_tpgid(&stat)
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_tpgid(_pid: u32) -> Option<i64> {
    None
}

/// Field 8 (`tpgid`) of one `/proc/<pid>/stat` line.
#[cfg(target_os = "linux")]
fn parse_stat_tpgid(stat: &str) -> Option<i64> {
    let after_comm = stat.rsplit_once(')')?.1;
    // This slice starts at field 3 (state), so tpgid at field 8 is index 5.
    after_comm.split_whitespace().nth(8 - 3)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn read_proc_start_time(pid: u32) -> Option<u64> {
    let stat =
        std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(22 - 3)?
        .parse()
        .ok()
}

/// True when `T3_STATUS_DEBUG` is set: the poller logs every detection pass
/// together with the facts that produced it.
///
/// This is the fastest way to explain a wrong badge — run the app with
/// `T3_STATUS_DEBUG=1` and the terminal shows exactly why each state changed.
pub fn debug_enabled() -> bool {
    std::env::var_os("T3_STATUS_DEBUG").is_some()
}

/// Compact state and evidence transitions, without screen contents.
pub fn trace_enabled() -> bool {
    std::env::var_os("T3_STATUS_TRACE").is_some()
}

/// Single status for a terminal, joining the screen itself.
///
/// The poller uses [`evaluate_screen_with_match`] with text it already built;
/// this keeps the pure, easy-to-test entry point used by the unit tests.
#[allow(dead_code)]
pub fn evaluate(input: &StatusInput<'_>, prev: SessionStatus, focused: bool) -> SessionStatus {
    evaluate_with_match(input, prev, focused).0
}

/// Like [`evaluate`], but also returns the agent rule match behind the verdict
/// (if any), so callers that log the explanation do not evaluate twice.
#[allow(dead_code)]
pub fn evaluate_with_match(
    input: &StatusInput<'_>,
    prev: SessionStatus,
    focused: bool,
) -> (SessionStatus, Option<agent_rules::RuleMatch>) {
    let screen = input.lines.join("\n");
    evaluate_screen_with_match(&screen, input, prev, focused)
}

/// [`evaluate_with_match`] for callers that already joined the screen.
///
/// The status poller scrapes the screen and needs the same joined text for the
/// temporal detector, the rule packs and the fallback rules. Passing it in
/// keeps a tick to one join instead of one per helper.
pub fn evaluate_screen_with_match(
    screen: &str,
    input: &StatusInput<'_>,
    prev: SessionStatus,
    focused: bool,
) -> (SessionStatus, Option<agent_rules::RuleMatch>) {
    let (detected, matched) = detect_full_screen(screen, input, prev);

    // Only an agent rule that says "working" is authoritative while focused.
    // Generic busy text (a prompt glyph, a stale "esc to interrupt") must not
    // keep a focused pane yellow: it is Active while you are looking at it and
    // Working once you switch away, matching Herdr's "a finish you watched is
    // idle" spirit. Agent reports and rule packs still win.
    let is_agent_working = matched
        .as_ref()
        .is_some_and(|m| m.state == RuleState::Working);

    // When the user is focused and interacting, plain shell commands report Active.
    // Only an agent running on its own ("own way") reports Working in a focused tab.
    let status = if detected == SessionStatus::Unknown {
        SessionStatus::Unknown
    } else if detected == SessionStatus::Working && !is_agent_working && focused {
        SessionStatus::Active
    } else {
        let focus_status = if focused {
            SessionStatus::Active
        } else {
            SessionStatus::Idle
        };
        if detected.rank() > focus_status.rank() {
            detected
        } else {
            focus_status
        }
    };
    (status, matched)
}

/// Like [`detect`], but also returns the rule match that produced the verdict
/// (when an agent pack matched), so debug logging does not have to evaluate
/// the pack a second time to explain the badge.
fn detect_full_screen(
    screen: &str,
    input: &StatusInput<'_>,
    prev: SessionStatus,
) -> (SessionStatus, Option<agent_rules::RuleMatch>) {
    let tail = tail_lines(screen, FallbackRules::WINDOW);

    if let Some(pack) = agent_rules::embedded().pack_for(input.foreground) {
        let matched = agent_rules::evaluate(
            pack,
            DetectionInput {
                screen,
                osc_title: input.title.unwrap_or(""),
                osc_progress: input.osc_progress,
            },
        );

        let status = match &matched {
            None => {
                if let Some(state) = input.detector_state {
                    match state {
                        State::AwaitingApproval => SessionStatus::Blocked,
                        State::Running => SessionStatus::Working,
                        State::Idle | State::Exited => SessionStatus::Unknown,
                    }
                } else if FallbackRules::is_blocked(&tail) {
                    SessionStatus::Blocked
                } else if FallbackRules::is_running(&tail) {
                    SessionStatus::Working
                } else {
                    SessionStatus::Unknown
                }
            }
            Some(m) if m.skip_state_update => prev,
            Some(m) => match m.state {
                RuleState::Blocked => SessionStatus::Blocked,
                RuleState::Working => SessionStatus::Working,
                RuleState::Idle => SessionStatus::Idle,
                // Unknown means "neither" — but the agent process really is
                // running, so report that rather than claiming the pane is idle.
                RuleState::Unknown => SessionStatus::Unknown,
            },
        };
        return (status, matched);
    }

    // No agent CLI is driving this terminal, so generic and fallback signals apply.
    let recent = FallbackRules::recent_lower(input, FallbackRules::WINDOW);
    if FallbackRules::error_lower(&recent) {
        return (SessionStatus::Error, None);
    }

    if let Some(state) = input.detector_state {
        match state {
            State::AwaitingApproval => return (SessionStatus::Blocked, None),
            State::Running => return (SessionStatus::Working, None),
            State::Idle | State::Exited => {}
        }
    }

    if FallbackRules::is_blocked(&tail) {
        return (SessionStatus::Blocked, None);
    }
    if input.detector_state.is_none() && FallbackRules::is_running(&tail) {
        return (SessionStatus::Working, None);
    }

    (
        if input.foreground.is_empty() {
            SessionStatus::Idle
        } else {
            SessionStatus::Unknown
        },
        None,
    )
}

/// Generic rules for terminals with no agent pack: a plain shell.
static GENERIC_REGEXES: LazyLock<(RegexSet, RegexSet)> =
    LazyLock::new(|| Profile::generic().compile());

/// Generic rules for terminals with no agent pack: a plain shell or uncataloged agent CLI.
struct FallbackRules;

impl FallbackRules {
    /// Lines considered "recent" for the fallback rules.
    const WINDOW: usize = 5;

    fn is_blocked(tail: &str) -> bool {
        GENERIC_REGEXES.0.is_match(tail)
    }

    fn is_running(tail: &str) -> bool {
        GENERIC_REGEXES.1.is_match(tail)
    }

    /// The newest `n` non-empty lines of the visible screen, trimmed and
    /// lowercased once so error rules share the normalized form.
    fn recent_lower(input: &StatusInput<'_>, n: usize) -> Vec<String> {
        let mut lines: Vec<String> = input
            .lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_lowercase())
            .collect();
        let excess = lines.len().saturating_sub(n);
        lines.split_off(excess)
    }

    /// Strong failure markers on the most recent output lines.
    ///
    /// Scans a small window rather than just the last line, because a shell
    /// redraws its prompt *below* the failed command's output.
    fn error_lower(recent: &[String]) -> bool {
        let start = recent.len().saturating_sub(4);
        recent[start..].iter().any(|line| {
            line.contains("command not found")
                || line.contains("permission denied")
                || line.contains("panicked at")
                || line.contains("segmentation fault")
                || line.contains("traceback (most recent call last)")
                || line.starts_with("error:")
                || line.starts_with("error ")
                || line.starts_with("fatal:")
                || line.starts_with("panic:")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(lines: &[&str], foreground: &[&str], focused: bool) -> SessionStatus {
        let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        let fg: Vec<String> = foreground.iter().map(|s| s.to_string()).collect();
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &fg,
            detector_state: None,
            osc_progress: "",
        };
        evaluate(&input, SessionStatus::Idle, focused)
    }

    /// A claude prompt box: two horizontal rules bracketing the input line.
    const CLAUDE_IDLE_BOX: &[&str] = &[
        "──────────────────────────────────",
        "  some conversation",
        "──────────────────────────────────",
        "❯ ",
        "──────────────────────────────────",
        "  ? for shortcuts",
    ];

    #[test]
    fn tail_lines_keeps_the_last_non_empty_lines() {
        assert_eq!(tail_lines("a\nb\n\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("only", 5), "only");
        assert_eq!(tail_lines("", 3), "");
        assert_eq!(tail_lines("a\nb\nc", 0), "");
        // Trailing whitespace is trimmed, blank lines are skipped.
        assert_eq!(tail_lines("one  \n\ntwo\t\n", 5), "one\ntwo");
    }

    #[test]
    fn priority_order() {
        assert!(SessionStatus::Error.rank() > SessionStatus::Blocked.rank());
        assert!(SessionStatus::Unknown.rank() > SessionStatus::Active.rank());
        assert!(SessionStatus::Done.rank() > SessionStatus::Unknown.rank());
        assert!(SessionStatus::Blocked.rank() > SessionStatus::Done.rank());
        assert!(SessionStatus::Done.rank() > SessionStatus::Working.rank());
        assert!(SessionStatus::Working.rank() > SessionStatus::Active.rank());
        assert!(SessionStatus::Active.rank() > SessionStatus::Idle.rank());
    }

    #[test]
    fn plain_shell_command_is_working() {
        // Process presence alone cannot prove that an unfamiliar command is active.
        assert_eq!(
            eval(&["building..."], &["cargo"], false),
            SessionStatus::Unknown
        );
    }

    #[test]
    fn shell_prompt_is_idle_without_foreground_process() {
        assert_eq!(
            eval(&["user@host ~/project ❯"], &[], false),
            SessionStatus::Idle
        );
        assert_eq!(eval(&[], &[], false), SessionStatus::Idle);
    }

    #[test]
    fn claude_at_its_prompt_is_idle_even_though_the_process_runs() {
        // The pack wins over the foreground fallback: an agent sitting at its
        // prompt is Idle even though its TUI is the running process.
        assert_eq!(
            eval(CLAUDE_IDLE_BOX, &["claude"], false),
            SessionStatus::Idle
        );
    }

    #[test]
    fn claude_working_footer_is_working() {
        assert_eq!(
            eval(
                &[
                    "⏺ Read(src/main.rs)",
                    "✳ Baking… (2m 14s · esc to interrupt)"
                ],
                &["claude"],
                false
            ),
            SessionStatus::Working
        );
    }

    #[test]
    fn claude_permission_menu_is_blocked() {
        assert_eq!(
            eval(
                &[
                    "──────────────────────────────────",
                    " Do you want to proceed?",
                    " ❯ 1. Yes",
                    "   2. No, and tell Claude what to do differently",
                    "──────────────────────────────────",
                    " esc to cancel · enter to confirm",
                ],
                &["claude"],
                false
            ),
            SessionStatus::Blocked
        );
    }

    #[test]
    fn agent_transcript_errors_do_not_paint_the_terminal_red() {
        // The code being discussed probably contains `error:`; that is not the
        // terminal failing, so the plain-shell Error rule stays out of the way
        // whenever a pack is active.
        assert_ne!(
            eval(
                &["error: could not compile `kathamo`", "❯ "],
                &["claude"],
                false
            ),
            SessionStatus::Error
        );
    }

    #[test]
    fn gemini_working_hint_is_not_a_blocker() {
        assert_eq!(
            eval(&[" ⠋ Thinking... (esc to cancel)"], &["gemini"], false),
            SessionStatus::Working
        );
    }

    #[test]
    fn agent_running_with_no_matching_rule_is_unknown() {
        // A process being present does not reveal whether the agent is waiting
        // for input or has completed its turn.
        assert_eq!(eval(&["hello"], &["claude"], false), SessionStatus::Unknown);
    }

    #[test]
    fn plain_shell_error_output_is_error() {
        assert_eq!(
            eval(&["bash: foo: command not found"], &[], false),
            SessionStatus::Error
        );
        assert_eq!(
            eval(&["fatal: not a git repository"], &[], false),
            SessionStatus::Error
        );
        // The shell redraws its prompt under the failure; the error must still
        // be visible in the window.
        assert_eq!(
            eval(
                &[
                    "error: could not compile `kathamo`",
                    "user@host ~/kathamo ❯"
                ],
                &[],
                false
            ),
            SessionStatus::Error
        );
    }

    #[test]
    fn plain_shell_confirmation_prompt_is_blocked() {
        assert_eq!(
            eval(&["Overwrite the file? [y/n]"], &["cp"], false),
            SessionStatus::Blocked
        );
    }

    #[test]
    fn screen_evaluator_does_not_mask_fresh_state_after_completion() {
        let lines = vec!["$".to_string()];
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &[],
            detector_state: None,
            osc_progress: "",
        };
        assert_eq!(
            evaluate(&input, SessionStatus::Done, false),
            SessionStatus::Idle
        );
        assert_eq!(
            evaluate(&input, SessionStatus::Done, true),
            SessionStatus::Active
        );
        assert_eq!(
            evaluate(&input, SessionStatus::Error, false),
            SessionStatus::Idle
        );
    }

    #[test]
    fn active_wins_when_nothing_else() {
        assert_eq!(eval(&["$"], &[], true), SessionStatus::Active);
        assert_eq!(eval(&["$"], &[], false), SessionStatus::Idle);
    }

    #[test]
    fn blocked_beats_active() {
        assert_eq!(
            eval(&["Delete everything? [y/n]"], &["rm"], true),
            SessionStatus::Blocked
        );
    }

    #[test]
    fn agent_names_come_from_the_packs() {
        assert!(is_agent_process("claude"));
        assert!(is_agent_process("claude-code"));
        assert!(is_agent_process("/usr/local/bin/codex"));
        assert!(is_agent_process("gemini"));
        assert!(is_agent_process("opencode"));
        assert!(!is_agent_process("claudette"));
        assert!(!is_agent_process("bash"));
        assert!(!is_agent_process("sleep"));
    }

    #[test]
    fn explain_names_the_matching_rule() {
        let lines: Vec<String> = ["✳ Baking… (2m 14s · esc to interrupt)"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let fg = vec!["claude".to_string()];
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &fg,
            detector_state: None,
            osc_progress: "",
        };
        let m = explain(&input).expect("a rule should match");
        assert_eq!(m.pack, "claude");
        assert_eq!(m.state.label(), "working");
        assert!(m.pack_version.is_some());
    }

    #[test]
    fn pack_lookup_reports_the_agent() {
        assert_eq!(pack_for(&["opencode".to_string()]), Some("opencode"));
        assert_eq!(pack_for(&["bash".to_string()]), None);
    }

    #[test]
    fn missing_process_metadata_is_marked_unknown() {
        assert_eq!(
            unknown_foreground_names().as_ref(),
            &["<process-state-unavailable>"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_names_refresh_when_children_change_in_same_group() {
        let mut cache = HashMap::new();
        let first = cache_foreground_names(
            &mut cache,
            41,
            72,
            vec![(41, Some(1)), (72, Some(2))],
            vec!["bash".into()],
        );
        let second = cache_foreground_names(
            &mut cache,
            41,
            72,
            vec![(41, Some(1)), (72, Some(2)), (73, Some(3))],
            vec!["agent".into(), "node".into()],
        );
        assert_eq!(first.as_ref(), &["bash"]);
        assert_eq!(second.as_ref(), &["agent", "node"]);
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tpgid_is_field_8_not_pgrp() {
        // Real line captured from the `portable-pty` fish shell this app spawns.
        // Field 5 (`pgrp`) equals the pid, which is why reading field 5 always
        // looked like "the shell is at a prompt"; field 8 (`tpgid`) is 380572,
        // the actual foreground job.
        let stat = "380159 (fish) S 379762 380159 380159 34822 380572 4194304 1445 \
                    5045 8 9 4 3 1 2 16 -4 1 0 5676308 300417024 2121 18446744073709551615";
        assert_eq!(parse_stat_tpgid(stat), Some(380572));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_instance_start_time_is_part_of_foreground_identity() {
        let mut cache = HashMap::new();
        let first = cache_foreground_names(
            &mut cache,
            41,
            72,
            vec![(72, Some(100))],
            vec!["agent".into()],
        );
        let replacement = cache_foreground_names(
            &mut cache,
            41,
            72,
            vec![(72, Some(200))],
            vec!["agent".into()],
        );
        assert!(!Arc::ptr_eq(&first, &replacement));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tpgid_parses_comm_with_spaces_and_parens() {
        // `comm` is user-controlled and may contain spaces and `)`, so the split
        // has to happen at the *last* paren, not the first.
        let stat = "42 (my (weird) comm) S 1 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 0 0 7";
        assert_eq!(parse_stat_tpgid(stat), Some(-1));
    }
    fn feed(d: &mut Detector, screen: &str, t0: Instant, ms: u64) -> State {
        d.update(screen, false, t0 + Duration::from_millis(ms))
    }

    #[test]
    fn running_then_idle() {
        let t0 = Instant::now();
        let mut d = Detector::new(Profile::generic(), t0);
        assert_eq!(
            feed(&mut d, "⠋ Thinking…\n  esc to interrupt", t0, 0),
            State::Running
        );
        // A static generic spinner stops being evidence; silence cannot prove done.
        assert_eq!(
            feed(&mut d, "⠋ Thinking…\n  esc to interrupt", t0, 5000),
            State::Idle
        );
        // done: plain prompt, then it stays unchanged
        assert_eq!(feed(&mut d, "All done.\n> ", t0, 5100), State::Running);
        assert_eq!(feed(&mut d, "All done.\n> ", t0, 7000), State::Idle);
    }

    #[test]
    fn stale_generic_spinner_expires_even_when_unrelated_output_changes() {
        let t0 = Instant::now();
        let mut d = Detector::new(Profile::generic(), t0);
        assert_eq!(feed(&mut d, "⠋ Thinking…\nclock: 0", t0, 0), State::Running);
        let screen = "⠋ Thinking…\nAll done.\nclock: 6\n> ";
        let state = feed(&mut d, screen, t0, 6000);
        assert_eq!(state, State::Idle);
        let lines = screen.lines().map(str::to_owned).collect::<Vec<_>>();
        let foreground = vec!["command-code".to_owned()];
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &foreground,
            detector_state: Some(state),
            osc_progress: "",
        };
        // Do not re-enable the expired spinner through a second regex path,
        // and do not mistake the expiration for a completed turn.
        assert_eq!(
            evaluate(&input, SessionStatus::Working, false),
            SessionStatus::Unknown
        );
    }

    #[test]
    fn changing_spinner_refreshes_generic_working_evidence() {
        let t0 = Instant::now();
        let mut d = Detector::new(Profile::generic(), t0);
        for ms in [0, 4000, 8000, 12000] {
            let screen = if ms % 8000 == 0 {
                "⠋ Thinking…"
            } else {
                "⠙ Thinking…"
            };
            assert_eq!(feed(&mut d, screen, t0, ms), State::Running);
        }
        assert_eq!(feed(&mut d, "⠙ Thinking…", t0, 18000), State::Idle);
    }

    #[test]
    fn authoritative_agent_rule_survives_generic_evidence_expiration() {
        let lines = vec!["✳ Baking… (2m 14s · esc to interrupt)".to_owned()];
        let foreground = vec!["claude".to_owned()];
        let input = StatusInput {
            lines: &lines,
            title: None,
            foreground: &foreground,
            detector_state: Some(State::Idle),
            osc_progress: "",
        };
        assert_eq!(
            evaluate(&input, SessionStatus::Working, false),
            SessionStatus::Working
        );
    }

    #[test]
    fn approval_needs_settled_screen() {
        let t0 = Instant::now();
        let mut d = Detector::new(Profile::generic(), t0);
        let s = "Run `rm -rf build`?\n❯ 1. Yes\n  2. No";
        assert_eq!(feed(&mut d, s, t0, 0), State::Running); // just appeared
        assert_eq!(feed(&mut d, s, t0, 600), State::AwaitingApproval);
        assert_eq!(feed(&mut d, "Continue? (y/n)", t0, 700), State::Running);
        assert_eq!(
            feed(&mut d, "Continue? (y/n)", t0, 1200),
            State::AwaitingApproval
        );
    }

    #[test]
    fn exit_and_custom_pattern() {
        let t0 = Instant::now();
        let p = Profile::generic().with_approval(r"trust this folder");
        let mut d = Detector::new(p, t0);
        let s = "Do you trust this folder?";
        feed(&mut d, s, t0, 0);
        assert_eq!(feed(&mut d, s, t0, 600), State::AwaitingApproval);
        assert_eq!(d.update(s, true, t0), State::Exited);
    }

    #[test]
    fn generic_agent_approval_patterns_are_blocked() {
        assert_eq!(
            eval(
                &["Do you want to proceed?", "❯ 1. Yes", "  2. No"],
                &[],
                false
            ),
            SessionStatus::Blocked
        );
        assert_eq!(
            eval(
                &["Allow tool execution `git commit`? (y/n)"],
                &["python3"],
                false
            ),
            SessionStatus::Blocked
        );
        assert_eq!(
            eval(&["Allow dangerous command? [y/n/a/d]"], &[], false),
            SessionStatus::Blocked
        );
        assert_eq!(
            eval(&["Press enter to continue"], &["aider"], false),
            SessionStatus::Blocked
        );
        assert_eq!(
            eval(&["Do you want to edit file? esc to cancel"], &[], false),
            SessionStatus::Blocked
        );
    }

    #[test]
    fn generic_agent_running_markers_are_working() {
        assert_eq!(eval(&["⠋ Thinking…"], &[], false), SessionStatus::Working);
        assert_eq!(
            eval(&["Generating response... (esc to interrupt)"], &[], false),
            SessionStatus::Working
        );
        assert_eq!(
            eval(&["◐ Processing request…"], &[], false),
            SessionStatus::Working
        );
    }

    #[test]
    fn detector_state_reflected_in_evaluate() {
        let lines = vec!["All done.\n> ".to_string()];
        let input_running = StatusInput {
            lines: &lines,
            title: None,
            foreground: &[],
            detector_state: Some(State::Running),
            osc_progress: "",
        };
        assert_eq!(
            evaluate(&input_running, SessionStatus::Idle, false),
            SessionStatus::Working
        );

        let input_approval = StatusInput {
            lines: &lines,
            title: None,
            foreground: &[],
            detector_state: Some(State::AwaitingApproval),
            osc_progress: "",
        };
        assert_eq!(
            evaluate(&input_approval, SessionStatus::Idle, false),
            SessionStatus::Blocked
        );
    }
    #[test]
    fn focus_does_not_turn_ambiguous_foreground_program_into_active() {
        // A live process with no reliable state stays unknown, even when focused.
        assert_eq!(
            eval(&["building..."], &["cargo"], true),
            SessionStatus::Unknown
        );
        assert_eq!(
            eval(&["user@host ❯ git status"], &["git"], true),
            SessionStatus::Unknown
        );
        assert_eq!(
            eval(&["user@host ❯ echo hello"], &[], true),
            SessionStatus::Active
        );
    }

    #[test]
    fn focused_agent_rule_working_is_still_working() {
        // A real agent pack that reports Working wins even while focused.
        assert_eq!(
            eval(
                &[
                    "⏺ Read(src/main.rs)",
                    "✳ Baking… (2m 14s · esc to interrupt)"
                ],
                &["claude"],
                true
            ),
            SessionStatus::Working
        );
    }

    #[test]
    fn generic_busy_text_is_active_while_focused_and_working_when_away() {
        // No agent pack: generic busy text is not authoritative. Looking at it
        // reads Active; switching away reads Working.
        let lines = ["⠋ Working... (3s · esc to interrupt)"];
        assert_eq!(eval(&lines, &[], true), SessionStatus::Active);
        assert_eq!(eval(&lines, &[], false), SessionStatus::Working);
    }
}
