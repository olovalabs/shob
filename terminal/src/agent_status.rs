//! OSC status reporting for agents running in the terminal.
//!
//! This scanner observes raw PTY output before the ANSI renderer consumes it.
//! It never removes or rewrites bytes sent to the renderer.

use std::collections::BTreeMap;

use base64::Engine as _;

const MAX_OSC_BYTES: usize = 4096;
const MAX_REPORTS: usize = 64;
const MAX_MESSAGE_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentReportState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl AgentReportState {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            _ => return None,
        })
    }

    fn priority(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Working => 1,
            Self::Error => 2,
            Self::Done => 3,
            Self::Idle => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatusReport {
    pub state: AgentReportState,
    pub id: String,
    pub message: Option<String>,
    pub kind: Option<String>,
    pub progress: Option<u8>,
    pub app: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    Escape,
    Osc,
    OscEscape,
    Ignore,
    IgnoreEscape,
}

#[derive(Debug)]
pub(crate) struct AgentOscScanner {
    state: ScanState,
    body: Vec<u8>,
    reports: BTreeMap<String, AgentStatusReport>,
    progress: String,
}

impl Default for AgentOscScanner {
    fn default() -> Self {
        Self {
            state: ScanState::Ground,
            body: Vec::with_capacity(128),
            reports: BTreeMap::new(),
            progress: String::new(),
        }
    }
}

impl AgentOscScanner {
    pub(crate) fn scan(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                ScanState::Ground => match byte {
                    0x1b => self.state = ScanState::Escape,
                    0x9d => self.begin_osc(),
                    _ => {}
                },
                ScanState::Escape => match byte {
                    b']' | 0x9d => self.begin_osc(),
                    b'c' => {
                        self.full_reset();
                        self.state = ScanState::Ground;
                    }
                    0x1b => self.state = ScanState::Escape,
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Osc => match byte {
                    0x07 | 0x9c => self.finish_osc(),
                    0x1b => self.state = ScanState::OscEscape,
                    _ => self.append(byte),
                },
                ScanState::OscEscape => match byte {
                    b'\\' => self.finish_osc(),
                    b']' | 0x9d => self.begin_osc(),
                    0x1b => self.state = ScanState::OscEscape,
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Ignore => match byte {
                    0x07 | 0x9c => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::IgnoreEscape,
                    _ => {}
                },
                ScanState::IgnoreEscape => match byte {
                    b'\\' => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::IgnoreEscape,
                    _ => self.state = ScanState::Ignore,
                },
            }
        }
    }

    pub(crate) fn resolved_report(&self) -> Option<AgentStatusReport> {
        self.reports
            .values()
            .min_by_key(|report| (report.state.priority(), report.id.as_str()))
            .cloned()
    }

    pub(crate) fn progress(&self) -> &str {
        &self.progress
    }

    pub(crate) fn clear_reports(&mut self) {
        self.reports.clear();
    }

    fn begin_osc(&mut self) {
        self.body.clear();
        self.state = ScanState::Osc;
    }

    fn append(&mut self, byte: u8) {
        if self.body.len() >= MAX_OSC_BYTES {
            self.body.clear();
            self.state = ScanState::Ignore;
        } else {
            self.body.push(byte);
        }
    }

    fn finish_osc(&mut self) {
        self.handle_body();
        self.body.clear();
        self.state = ScanState::Ground;
    }

    fn handle_body(&mut self) {
        if let Some(body) = self.body.strip_prefix(b"7501;") {
            let body = body.to_vec();
            self.handle_report(&body);
        } else if let Some(progress) = self.body.strip_prefix(b"9;4;") {
            if progress.len() <= 128 && progress.iter().all(|b| b.is_ascii_graphic() || *b == b';')
            {
                self.progress = format!("4;{}", String::from_utf8_lossy(progress));
            }
        } else if self.body.starts_with(b"133;A") {
            self.reports.retain(|_, report| {
                !matches!(
                    report.state,
                    AgentReportState::Working | AgentReportState::Blocked | AgentReportState::Idle
                )
            });
        }
    }

    fn handle_report(&mut self, body: &[u8]) {
        let Ok(body) = std::str::from_utf8(body) else {
            return;
        };
        if body.trim() == "?" || body.trim().starts_with("?:") {
            return;
        }
        let mut fields = BTreeMap::new();
        for pair in body.trim().split(':') {
            let Some((key, value)) = pair.split_once('=') else {
                return;
            };
            let key = key.trim();
            let value = value.trim();
            if !valid_key(key) || !valid_value(value) {
                return;
            }
            fields.insert(key, value);
        }
        let id = fields.get("id").copied().unwrap_or("");
        if !valid_id(id) {
            return;
        }
        if fields.get("state") == Some(&"clear") {
            self.reports.remove(id);
            return;
        }
        let Some(state) = fields.get("state").and_then(|s| AgentReportState::parse(s)) else {
            return;
        };
        let message = match fields.get("msg") {
            Some(encoded) => match decode_message(encoded) {
                Some(s) => Some(s),
                None => return,
            },
            None => None,
        };
        let kind = fields
            .get("kind")
            .filter(|kind| {
                state == AgentReportState::Blocked
                    && matches!(**kind, "permission" | "question" | "auth")
            })
            .map(|s| (*s).to_owned());
        let progress = fields
            .get("progress")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| {
                *value <= 100
                    && matches!(state, AgentReportState::Working | AgentReportState::Blocked)
            });
        let app = fields
            .get("app")
            .filter(|value| valid_segment(value))
            .map(|s| (*s).to_owned());

        if !self.reports.contains_key(id) && self.reports.len() >= MAX_REPORTS {
            let oldest = self.reports.keys().next().cloned();
            if let Some(oldest) = oldest {
                self.reports.remove(&oldest);
            }
        }
        self.reports.insert(
            id.to_owned(),
            AgentStatusReport {
                state,
                id: id.to_owned(),
                message,
                kind,
                progress,
                app,
            },
        );
    }

    fn full_reset(&mut self) {
        self.reports.clear();
        self.progress.clear();
        self.body.clear();
        self.state = ScanState::Ground;
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 16 && key.bytes().all(|b| b.is_ascii_lowercase())
}

fn valid_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}

fn valid_id(id: &str) -> bool {
    id.len() <= 128
        && id.split('/').count() <= 8
        && (id.is_empty() || id.split('/').all(valid_segment))
}

fn decode_message(encoded: &str) -> Option<String> {
    if encoded.len() > 2732 {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        .ok()?;
    if decoded.len() > MAX_MESSAGE_BYTES {
        return None;
    }
    let decoded = String::from_utf8(decoded).ok()?;
    Some(
        decoded
            .chars()
            .filter(|c| {
                !c.is_control()
                    && !matches!(*c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{feff}')
                    && !matches!(*c as u32, 0x200b..=0x200d | 0x202a..=0x202e | 0x2066..=0x2069)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn report(state: &str, id: &str, msg: &str) -> Vec<u8> {
        let msg = base64::engine::general_purpose::STANDARD.encode(msg);
        format!("\x1b]7501;state={state}:id={id}:msg={msg}\x1b\\").into_bytes()
    }

    #[test]
    fn scans_reports_split_across_chunks_and_preserves_resolution() {
        let mut scanner = AgentOscScanner::default();
        let bytes = report("working", "agent", "Working on it");
        scanner.scan(&bytes[..9]);
        assert!(scanner.resolved_report().is_none());
        scanner.scan(&bytes[9..]);
        let found = scanner.resolved_report().unwrap();
        assert_eq!(found.state, AgentReportState::Working);
        assert_eq!(found.message.as_deref(), Some("Working on it"));
    }

    #[test]
    fn blocker_wins_and_clear_restores_other_report() {
        let mut scanner = AgentOscScanner::default();
        scanner.scan(&report("working", "a", ""));
        scanner.scan(&report("blocked", "b", "Approve this"));
        assert_eq!(
            scanner.resolved_report().unwrap().state,
            AgentReportState::Blocked
        );
        scanner.scan(b"\x1b]7501;state=clear:id=b\x07");
        let found = scanner.resolved_report().unwrap();
        assert_eq!(found.id, "a");
        assert_eq!(found.state, AgentReportState::Working);
    }

    #[test]
    fn parses_progress_and_osc_133_prompt_lifecycle() {
        let mut scanner = AgentOscScanner::default();
        scanner.scan(b"\x1b]9;4;1;40\x07\x1b]7501;state=working\x07");
        assert_eq!(scanner.progress(), "4;1;40");
        scanner.scan(b"\x1b]133;A\x07");
        assert!(scanner.resolved_report().is_none());
    }

    #[test]
    fn supports_bel_c1_and_reset_and_ignores_oversized_sequences() {
        let mut scanner = AgentOscScanner::default();
        scanner.scan(b"\x9d7501;state=done\x9c");
        assert_eq!(
            scanner.resolved_report().unwrap().state,
            AgentReportState::Done
        );
        scanner.scan(b"\x1bc");
        assert!(scanner.resolved_report().is_none());
        scanner.scan(&[b'\x1b', b']']);
        scanner.scan(&vec![b'x'; MAX_OSC_BYTES + 1]);
        scanner.scan(b"7501;state=blocked\x07");
        assert!(scanner.resolved_report().is_none());
    }

    #[test]
    fn validates_ids_and_limits_report_count() {
        let mut scanner = AgentOscScanner::default();
        scanner.scan(b"\x1b]7501;state=working:id=bad!\x07");
        assert!(scanner.resolved_report().is_none());
        for id in 0..=MAX_REPORTS {
            scanner.scan(format!("\x1b]7501;state=idle:id={id}\x07").as_bytes());
        }
        assert_eq!(scanner.reports.len(), MAX_REPORTS);
    }
}
