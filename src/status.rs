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

#[cfg(target_os = "linux")]
type ForegroundCache = HashMap<u32, (i64, Vec<(u32, Option<u64>)>, Arc<[String]>)>;

/// Foreground-group cache: shell pid -> (tpgid, shared process-name snapshot).
///
/// The poller runs every 500 ms per session, but the foreground group only
/// changes when a command starts or ends. The `stat` read still happens each
/// tick (one small read, and it is the change detector), while the heavier
/// `comm` / `cmdline` / `children` reads are skipped while the group is
/// unchanged. Snapshots are shared across the status and logo paths; closed
/// panes explicitly remove their entry.
#[cfg(target_os = "linux")]
static FG_CACHE: LazyLock<parking_lot::Mutex<ForegroundCache>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

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
    Changing,
    Idle,
    AwaitingApproval,
    Exited,
}

impl State {
    pub fn to_session_status(self) -> SessionStatus {
        match self {
            State::Running => SessionStatus::Working,
            State::Changing => SessionStatus::Unknown,
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
    settling: bool,
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
            settling: true,
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
        self.settling = stable < self.idle_after;
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
        // Screen changes alone are weaker evidence than a busy marker.
        if stable < self.idle_after {
            State::Changing
        } else {
            State::Idle
        }
    }

    pub fn settle(&self) -> Duration {
        self.settle
    }

    pub fn is_settling(&self) -> bool {
        self.settling
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
    cache: &mut ForegroundCache,
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
                        State::Running | State::Changing => SessionStatus::Working,
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
            State::Changing | State::Idle | State::Exited => {}
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
#[path = "status/tests.rs"]
mod tests;
