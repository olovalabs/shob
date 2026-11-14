//! Footer component for TUI.
//!
//! Based on TypeScript implementation:
//! `packages/coding-agent/src/modes/interactive/components/footer.ts`.
//!
//! Upstream's footer is a two-line status bar:
//!
//! ```text
//! ~/proj/pi-rust (main) • my-session
//! ↑12k ↓3.4k R40k W1.2k CH72.0% $0.421 63.2%/200k (auto)   [model • medium]
//! ```
//!
//! * line 1 — the working directory (home-relative), the git branch, and the
//!   session name, all dimmed;
//! * line 2 — usage totals (`↑input ↓output RcacheRead WcacheWrite CHhit%`),
//!   cost, and context-window usage on the left, with the model (and thinking
//!   level) right-aligned, mirroring pi.
//!
//! The editor's own bottom edge provides separation, so the footer draws no
//! extra rule.

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::theme::theme;
use crate::utils::{truncate_to_width, visible_width};

/// Usage totals aggregated across the session (native `usage-totals.ts`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost: f64,
}

impl UsageTotals {
    pub fn add(&mut self, input: i64, output: i64, cache_read: i64, cache_write: i64, cost: f64) {
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
        self.cost += cost;
    }
}

/// Format token counts for compact footer display (native `formatTokens`).
pub fn format_tokens(count: i64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1_000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1_000.0).round() as i64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as i64)
}

/// Render a cwd home-relative (`~` prefix), matching native `formatCwdForFooter`.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return cwd.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if cwd == home {
        return "~".to_string();
    }
    for sep in ['/', '\\'] {
        let prefix = format!("{home}{sep}");
        if let Some(rest) = cwd.strip_prefix(&prefix) {
            return format!("~{sep}{rest}");
        }
    }
    cwd.to_string()
}

fn sanitize(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Footer component that displays status information.
pub struct FooterComponent {
    /// Current status text (transient run state).
    status: Mutex<String>,
    /// Extension-provided status text (e.g. `langfuse ✓ (trace sent)`), driven
    /// by the host's `SetStatus` runtime action. Rendered after the run status.
    extension_status: Mutex<String>,
    /// Model name.
    model: Mutex<String>,
    /// Keybinding hints (shown only when there is no stats content).
    hints: Mutex<String>,
    /// Optional thinking-level suffix shown after the model (pi parity:
    /// `model • thinking off` / `model • medium`). `None` → not shown.
    thinking_level: Mutex<Option<String>>,
    /// Working directory (unmodified).
    cwd: Mutex<Option<String>>,
    /// Home directory for `~`-relative rendering.
    home: Mutex<Option<String>>,
    /// Git branch, when inside a repository.
    git_branch: Mutex<Option<String>>,
    /// Session name.
    session_name: Mutex<Option<String>>,
    /// Aggregated usage totals.
    usage: Mutex<UsageTotals>,
    /// Latest prompt-cache hit rate (0..=100), when known.
    cache_hit_rate: Mutex<Option<f64>>,
    /// Context usage percent (0..=100), when known.
    context_percent: Mutex<Option<f64>>,
    /// Latest context-token count reported by an assistant response, when
    /// known. The percent is derived from this against `context_window`; `None`
    /// renders `?` (no post-compaction usage yet, or nothing reported).
    context_tokens: Mutex<Option<i64>>,
    /// Model context window in tokens (0 ⇒ unknown).
    context_window: Mutex<i64>,
    /// Whether auto-compaction is enabled (renders the `(auto)` suffix).
    auto_compact: Mutex<bool>,
}

impl FooterComponent {
    /// Create a new footer component.
    pub fn new() -> Self {
        Self {
            status: Mutex::new(String::new()),
            extension_status: Mutex::new(String::new()),
            model: Mutex::new("claude-sonnet-5".to_string()),
            hints: Mutex::new("Enter send · Ctrl+C abort · /help".to_string()),
            thinking_level: Mutex::new(None),
            cwd: Mutex::new(None),
            home: Mutex::new(
                std::env::var("HOME")
                    .ok()
                    .or_else(|| std::env::var("USERPROFILE").ok()),
            ),
            git_branch: Mutex::new(None),
            session_name: Mutex::new(None),
            usage: Mutex::new(UsageTotals::default()),
            cache_hit_rate: Mutex::new(None),
            context_percent: Mutex::new(None),
            context_tokens: Mutex::new(None),
            context_window: Mutex::new(0),
            auto_compact: Mutex::new(true),
        }
    }

    /// Set the status text.
    pub fn set_status(&self, status: &str) {
        if let Ok(mut s) = self.status.lock() {
            *s = status.to_string();
        }
    }

    /// Set the extension status text (empty clears it). Kept separate from
    /// [`set_status`](Self::set_status) so a run-state transition that clears
    /// the run status does not wipe what an extension reported.
    pub fn set_extension_status(&self, status: &str) {
        if let Ok(mut s) = self.extension_status.lock() {
            *s = status.to_string();
        }
    }

    /// Set the model name.
    pub fn set_model(&self, model: &str) {
        if let Ok(mut m) = self.model.lock() {
            *m = model.to_string();
        }
    }

    /// Set the keybinding hints.
    pub fn set_hints(&self, hints: &str) {
        if let Ok(mut h) = self.hints.lock() {
            *h = hints.to_string();
        }
    }

    /// Set the thinking-level suffix shown after the model name (pi parity).
    pub fn set_thinking_level(&self, level: Option<&str>) {
        if let Ok(mut t) = self.thinking_level.lock() {
            *t = level.map(|s| s.to_string());
        }
    }

    /// Clear the thinking-level suffix.
    pub fn clear_thinking_level(&self) {
        if let Ok(mut t) = self.thinking_level.lock() {
            *t = None;
        }
    }

    /// Set the working directory (line 1).
    pub fn set_cwd(&self, cwd: &str) {
        if let Ok(mut c) = self.cwd.lock() {
            *c = Some(cwd.to_string());
        }
    }

    /// Set the git branch shown next to the pwd (line 1).
    pub fn set_git_branch(&self, branch: Option<&str>) {
        if let Ok(mut b) = self.git_branch.lock() {
            *b = branch.map(|s| s.to_string());
        }
    }

    /// Set the session name shown after the branch (line 1).
    pub fn set_session_name(&self, name: Option<&str>) {
        if let Ok(mut n) = self.session_name.lock() {
            *n = name.map(|s| s.to_string());
        }
    }

    /// Replace the aggregated usage totals (line 2).
    pub fn set_usage(&self, totals: UsageTotals) {
        if let Ok(mut u) = self.usage.lock() {
            *u = totals;
        }
    }

    /// Add one turn's usage to the running totals (line 2).
    pub fn add_usage(&self, input: i64, output: i64, cache_read: i64, cache_write: i64, cost: f64) {
        if let Ok(mut u) = self.usage.lock() {
            u.add(input, output, cache_read, cache_write, cost);
        }
    }

    /// Set the latest cache hit rate percent (line 2), or `None` to hide.
    pub fn set_cache_hit_rate(&self, rate: Option<f64>) {
        if let Ok(mut r) = self.cache_hit_rate.lock() {
            *r = rate;
        }
    }

    /// Set the context usage percent + window (line 2).
    pub fn set_context_usage(&self, percent: Option<f64>, window: i64) {
        if let Ok(mut p) = self.context_percent.lock() {
            *p = percent;
        }
        if let Ok(mut w) = self.context_window.lock() {
            *w = window;
        }
    }

    /// Record the context-token count reported by the latest assistant
    /// response and recompute the displayed `%`. Pass `None` to reset the
    /// badge to `?` — e.g. after a compaction, when the last usage describes
    /// the pre-compaction context (mirrors pi's `percent: null`).
    pub fn set_context_tokens(&self, tokens: Option<i64>) {
        if let Ok(mut t) = self.context_tokens.lock() {
            *t = tokens;
        }
        self.recompute_context_percent();
    }

    /// Set the model's context window and recompute the `%` against the
    /// tracked token count (a model switch changes the denominator).
    pub fn set_context_window(&self, window: i64) {
        if let Ok(mut w) = self.context_window.lock() {
            *w = window;
        }
        self.recompute_context_percent();
    }

    /// Drop every per-session counter so a new session starts from zero.
    ///
    /// The footer accumulates usage across turns (`add_usage`) and keeps the
    /// last reported context size, so without this a `/new` session inherits
    /// the previous one's `↑input ↓output` totals and `%/window` badge and
    /// appears to keep adding to them.
    ///
    /// Deliberately does NOT touch `context_window`, `model`, `cwd` or
    /// `git_branch`: those describe the environment and the active model, not
    /// the session's history.
    pub fn reset_session_counters(&self) {
        self.set_usage(UsageTotals::default());
        self.set_cache_hit_rate(None);
        // Recomputes the badge to `?` because there is no token count left.
        self.set_context_tokens(None);
        self.set_session_name(None);
        self.set_status("");
    }

    /// Derive the context-usage percent from the tracked tokens/window.
    fn recompute_context_percent(&self) {
        let window = *self.context_window.lock().unwrap();
        let tokens = *self.context_tokens.lock().unwrap();
        let percent = match (tokens, window) {
            (Some(t), w) if t > 0 && w > 0 => Some(t as f64 / w as f64 * 100.0),
            _ => None,
        };
        if let Ok(mut p) = self.context_percent.lock() {
            *p = percent;
        }
    }

    /// Set whether auto-compaction is enabled (renders `(auto)`).
    pub fn set_auto_compact(&self, enabled: bool) {
        if let Ok(mut a) = self.auto_compact.lock() {
            *a = enabled;
        }
    }

    /// Get the status text.
    pub fn get_status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    /// Get the model name.
    pub fn get_model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// Build the line-1 pwd/branch/session string (uncolored).
    fn pwd_line(&self) -> String {
        let cwd = self.cwd.lock().unwrap().clone();
        let home = self.home.lock().unwrap().clone();
        let branch = self.git_branch.lock().unwrap().clone();
        let name = self.session_name.lock().unwrap().clone();

        let mut parts = Vec::new();
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            parts.push(format_cwd_for_footer(&cwd, home.as_deref()));
        }
        if let Some(branch) = branch.filter(|b| !b.is_empty()) {
            if let Some(last) = parts.last_mut() {
                *last = format!("{last} ({branch})");
            } else {
                parts.push(format!("({branch})"));
            }
        }
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            parts.push(name);
        }
        parts.join(" • ")
    }

    /// Build the line-2 stats-left string; returns `(plain, styled)`.
    fn stats_line(&self) -> (String, String) {
        let colors = theme().colors;
        let usage = *self.usage.lock().unwrap();
        let cache_hit = *self.cache_hit_rate.lock().unwrap();
        let context_percent = *self.context_percent.lock().unwrap();
        let context_window = *self.context_window.lock().unwrap();
        let auto = *self.auto_compact.lock().unwrap();

        let mut parts: Vec<String> = Vec::new();
        if usage.input > 0 {
            parts.push(format!("↑{}", format_tokens(usage.input)));
        }
        if usage.output > 0 {
            parts.push(format!("↓{}", format_tokens(usage.output)));
        }
        if usage.cache_read > 0 {
            parts.push(format!("R{}", format_tokens(usage.cache_read)));
        }
        if usage.cache_write > 0 {
            parts.push(format!("W{}", format_tokens(usage.cache_write)));
        }
        if usage.cache_read > 0 || usage.cache_write > 0 {
            if let Some(rate) = cache_hit {
                parts.push(format!("CH{rate:.1}%"));
            }
        }
        if usage.cost > 0.0 {
            parts.push(format!("${:.3}", usage.cost));
        }

        // Context usage: colorized by pressure like upstream.
        let auto_indicator = if auto { " (auto)" } else { "" };
        let context_display = match context_percent {
            Some(p) => format!("{p:.1}%/{}{auto_indicator}", format_tokens(context_window)),
            None if context_window > 0 => {
                format!("0.0%/{}{auto_indicator}", format_tokens(context_window))
            }
            None => String::new(),
        };
        let context_styled = if context_display.is_empty() {
            String::new()
        } else {
            let color = match context_percent {
                Some(p) if p > 90.0 => colors.error,
                Some(p) if p > 70.0 => colors.warning,
                _ => colors.muted,
            };
            color.fg(&context_display)
        };

        let plain = if context_display.is_empty() {
            parts.join(" ")
        } else if parts.is_empty() {
            context_display.clone()
        } else {
            format!("{} {}", parts.join(" "), context_display)
        };

        let mut styled = parts.join(" ");
        if !context_styled.is_empty() {
            if styled.is_empty() {
                styled = context_styled;
            } else {
                styled = format!("{styled} {context_styled}");
            }
        }
        (plain, styled)
    }
}

impl Default for FooterComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for FooterComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let status = self.status.lock().unwrap().clone();
        let extension_status = self.extension_status.lock().unwrap().clone();
        let model = self.model.lock().unwrap().clone();
        let hints = self.hints.lock().unwrap().clone();
        let thinking_level = self.thinking_level.lock().unwrap().clone();

        // ---- Line 1: pwd (branch) • session ----
        let pwd = self.pwd_line();
        let mut lines: Vec<String> = Vec::new();
        if !pwd.is_empty() {
            let styled = colors.dim.fg(&pwd);
            let row = if visible_width(&styled) > width {
                truncate_to_width(&styled, width, "…")
            } else {
                styled
            };
            lines.push(row);
        }

        // ---- Line 2: stats … model ----
        let model_label = if !model.is_empty() {
            let inner = match thinking_level {
                Some(ref lvl) if lvl == "off" => format!("{model} • thinking off"),
                Some(ref lvl) => format!("{model} • {lvl}"),
                None => model.clone(),
            };
            colors.dim.fg(&inner)
        } else {
            String::new()
        };

        let status_word = if status.is_empty() {
            String::new()
        } else {
            let sanitized = sanitize(&status);
            let lower = sanitized.to_ascii_lowercase();
            let color = if lower.contains("abort") {
                colors.error
            } else if lower.contains("work") || lower.contains("running") {
                colors.accent
            } else {
                colors.muted
            };
            format!(" {}", color.fg(&sanitized))
        };

        // Extension status (e.g. the langfuse plugin) sits after the run status
        // and is dimmed so it never competes with `Aborting…`/hints.
        let extension_word = if extension_status.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", colors.dim.fg(&sanitize(&extension_status)))
        };

        let (stats_plain, stats_styled) = self.stats_line();
        let has_stats = !stats_plain.is_empty();

        // Left = usage stats (or keybinding hints on a fresh session) + status;
        // right = the model, right-aligned (pi layout).
        let left_styled = if has_stats {
            format!("{stats_styled}{status_word}{extension_word}")
        } else {
            format!("{}{status_word}{extension_word}", colors.dim.fg(&hints))
        };
        let right_styled = model_label;

        let left_w = visible_width(&left_styled);
        let right_w = visible_width(&right_styled);
        let min_padding = 2;

        let row = if left_w == 0 && right_w == 0 {
            String::new()
        } else if left_w + min_padding + right_w <= width {
            let pad = " ".repeat(width.saturating_sub(left_w + right_w));
            format!("{left_styled}{pad}{right_styled}")
        } else if right_w == 0 {
            if left_w > width {
                truncate_to_width(&left_styled, width, "…")
            } else {
                left_styled
            }
        } else if left_w + min_padding <= width {
            let available = width - left_w - min_padding;
            let truncated_right = truncate_to_width(&right_styled, available, "");
            let rw = visible_width(&truncated_right);
            let pad = " ".repeat(width.saturating_sub(left_w + rw));
            format!("{left_styled}{pad}{truncated_right}")
        } else {
            // No room for the model; show stats truncated.
            if left_w > width {
                truncate_to_width(&left_styled, width, "…")
            } else {
                left_styled
            }
        };

        lines.push(row);
        lines
    }

    fn invalidate(&self) {
        // Footer has no cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    /// The extension status (langfuse ✓ …) renders after the run status and is
    /// independent of it: clearing the run status must not clear it.
    #[test]
    fn test_footer_with_extension_status() {
        let footer = FooterComponent::new();
        footer.set_status("Aborting…");
        footer.set_extension_status("langfuse ✓ (trace sent)");
        let row = strip_ansi(&footer.render(120).join(" "));
        assert!(row.contains("Aborting…"), "run status lost: {row}");
        assert!(
            row.contains("langfuse ✓ (trace sent)"),
            "ext status lost: {row}"
        );

        // A run-status transition that clears the run status keeps the
        // extension line (the two are separate slots).
        footer.set_status("");
        let row = strip_ansi(&footer.render(120).join(" "));
        assert!(!row.contains("Aborting…"));
        assert!(
            row.contains("langfuse ✓ (trace sent)"),
            "ext status lost: {row}"
        );

        // Clearing the extension status removes it again.
        footer.set_extension_status("");
        let row = strip_ansi(&footer.render(120).join(" "));
        assert!(!row.contains("langfuse"), "ext status not cleared: {row}");
    }

    #[test]
    fn test_footer_basic() {
        let footer = FooterComponent::new();
        let lines = footer.render(80);
        // No cwd set ⇒ single stats row.
        assert_eq!(lines.len(), 1);
        assert!(strip_ansi(&lines[0]).contains("Ctrl+C"));
    }

    #[test]
    fn test_footer_with_status() {
        let footer = FooterComponent::new();
        footer.set_status("Working…");
        footer.set_model("claude-sonnet-5");
        let lines = footer.render(80);
        let row = strip_ansi(&lines[0]);
        assert!(row.contains("Working…"));
        assert!(row.contains("claude-sonnet-5"));
    }

    #[test]
    fn test_footer_truncate() {
        let footer = FooterComponent::new();
        footer.set_status("This is a very long status message that should be truncated");
        footer.set_model("claude-opus-4");
        footer.set_hints("Ctrl+C: Exit | Shift+Enter: Send | Ctrl+L: Clear | More hints here");
        let lines = footer.render(40);
        for line in &lines {
            assert!(
                visible_width(line) <= 40,
                "row too wide: {}",
                visible_width(line)
            );
        }
    }

    #[test]
    fn test_footer_thinking_level_suffix() {
        let footer = FooterComponent::new();
        footer.set_model("test-model");
        footer.set_thinking_level(Some("medium"));
        let row = strip_ansi(&footer.render(80)[0]);
        assert!(row.contains("test-model • medium"), "suffix missing: {row}");
        assert!(!row.contains("[test-model"), "unexpected brackets: {row}");
    }

    #[test]
    fn test_two_line_with_cwd_and_usage() {
        let footer = FooterComponent::new();
        footer.set_cwd("/home/user/proj");
        footer.set_git_branch(Some("main"));
        footer.set_usage(UsageTotals {
            input: 12_300,
            output: 3_400,
            cache_read: 40_000,
            cache_write: 1_200,
            cost: 0.421,
        });
        footer.set_cache_hit_rate(Some(72.0));
        footer.set_context_usage(Some(63.2), 200_000);
        let lines = footer.render(120);
        assert_eq!(lines.len(), 2, "expected pwd + stats rows");
        let pwd = strip_ansi(&lines[0]);
        assert!(pwd.contains("(main)"), "pwd line: {pwd}");
        let stats = strip_ansi(&lines[1]);
        assert!(stats.contains("↑12k"), "stats: {stats}");
        assert!(stats.contains("$0.421"), "stats: {stats}");
        assert!(stats.contains("63.2%/200k"), "stats: {stats}");
        assert!(stats.contains("(auto)"), "stats: {stats}");
    }

    #[test]
    fn context_badge_derives_percent_from_reported_tokens() {
        let footer = FooterComponent::new();
        footer.set_context_window(512_000);
        // No response yet → shows 0.0% initially.
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("0.0%/512k"), "stats: {stats}");

        // A reported token count fills in the percent against the window.
        footer.set_context_tokens(Some(256_000));
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("50.0%/512k"), "stats: {stats}");

        // Switching the model rescales the same token count.
        footer.set_context_window(128_000);
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("200.0%/128k"), "stats: {stats}");

        // `None` (e.g. right after a compaction) resets to 0.0%.
        footer.set_context_tokens(None);
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("0.0%/128k"), "stats: {stats}");
    }

    /// A new session must not inherit the previous session's counters, but the
    /// environment/model facts must survive.
    #[test]
    fn reset_session_counters_clears_usage_and_keeps_environment() {
        let footer = FooterComponent::new();
        footer.set_model("(prov) model");
        footer.set_cwd("/home/u/proj");
        footer.set_git_branch(Some("main"));
        footer.set_context_window(200_000);
        footer.set_session_name(Some("old session"));
        footer.set_usage(UsageTotals {
            input: 12_300,
            output: 3_400,
            cache_read: 40_000,
            cache_write: 1_200,
            cost: 0.421,
        });
        footer.set_cache_hit_rate(Some(72.0));
        footer.set_context_tokens(Some(126_400));

        let before = strip_ansi(&footer.render(120).join("\n"));
        assert!(before.contains("↑12k"), "before: {before}");
        assert!(before.contains("$0.421"), "before: {before}");
        assert!(before.contains("old session"), "before: {before}");
        assert!(before.contains("63.2%/200k"), "before: {before}");

        footer.reset_session_counters();

        let after = strip_ansi(&footer.render(120).join("\n"));
        // Session-scoped counters are gone.
        assert!(!after.contains("↑12k"), "usage must reset: {after}");
        assert!(!after.contains("$0.421"), "cost must reset: {after}");
        assert!(!after.contains("old session"), "name must reset: {after}");
        assert!(!after.contains("63.2%"), "badge must reset: {after}");
        assert!(!after.contains("72.0%"), "cache rate must reset: {after}");
        // Environment/model facts survive: they describe the session's setup,
        // not its history.
        assert!(after.contains("(prov) model"), "model kept: {after}");
        assert!(after.contains("(main)"), "branch kept: {after}");
        assert!(after.contains("proj"), "cwd kept: {after}");
        assert!(after.contains("200k"), "window kept: {after}");
    }

    #[test]
    fn format_tokens_compacts() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(40_000), "40k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn cwd_home_relative() {
        assert_eq!(
            format_cwd_for_footer("/home/u/proj", Some("/home/u")),
            "~/proj"
        );
        assert_eq!(format_cwd_for_footer("/home/u", Some("/home/u")), "~");
        assert_eq!(format_cwd_for_footer("/other", Some("/home/u")), "/other");
        assert_eq!(format_cwd_for_footer("/x", None), "/x");
    }
}
