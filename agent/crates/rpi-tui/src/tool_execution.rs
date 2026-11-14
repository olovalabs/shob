//! Tool execution component for displaying tool calls.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/tool-execution.ts

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::component::{Component, RenderedDocument};
use crate::ansi::{bold, strip_ansi};
use crate::theme::{theme, theme_revision, Theme, ThemeColors};
use crate::tool_panel::ToolPanel;
use crate::utils::{apply_background_to_line, truncate_to_width, wrap_text_with_ansi};

/// Maximum diff lines rendered inline when expanded before collapsing the rest.
const DIFF_LINE_CAP: usize = 40;
/// Diff lines shown when collapsed — a preview density that keeps a big edit
/// from dumping 40 lines into the transcript while still showing what changed.
/// Ctrl+O expands to [`DIFF_LINE_CAP`].
const DIFF_PREVIEW_LINES: usize = 6;
/// Maximum visual rows shown for a regular tool result while collapsed.
/// Read/grep/find output can be hundreds of physical lines; keeping a compact
/// preview prevents one tool from taking over the conversation transcript.
const OUTPUT_PREVIEW_LINES: usize = 12;

/// Tool execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool is pending execution
    Pending,
    /// Tool is running
    Running,
    /// Tool completed successfully
    Completed,
    /// Tool failed
    Failed,
}

/// Component that displays a tool execution.
///
/// Mirrors TypeScript ToolExecutionComponent class (simplified).
pub struct ToolExecutionComponent {
    /// Tool name
    name: Mutex<String>,
    /// Optional display title. The raw name is retained for argument parsing
    /// while extensions such as skill loading can provide a clearer label.
    display_title: Mutex<Option<String>>,
    /// When set, the component renders in the native-Pi skill-invocation
    /// style: a `[skill] <name>` box on the custom-message background that
    /// collapses to a single line and expands to the full skill markdown
    /// content (the `read` result). Mirrors pi's `SkillInvocationMessageComponent`.
    skill_name: Mutex<Option<String>>,
    /// Tool arguments (displayed)
    args: Mutex<String>,
    /// Tool result (displayed after execution)
    result: Mutex<Option<String>>,
    /// Execution status
    status: Mutex<ToolStatus>,
    /// When the tool started running (for elapsed time display)
    started_at: Mutex<Option<Instant>>,
    /// When the tool finished (for total elapsed time display)
    finished_at: Mutex<Option<Instant>>,
    /// Whether expanded
    expanded: Mutex<bool>,
    /// Optional pre-rendered colored diff lines (from `render_diff`). When
    /// present the diff is always shown regardless of `expanded` — the diff
    /// IS the useful content for an `edit` tool.
    diff_lines: Mutex<Option<Vec<String>>>,
    /// Render the result body as markdown (tables, bold, lists) instead of
    /// plain text. Tools opt in by setting `details.markdown = true` on their
    /// result; the interactive host calls [`Self::set_result_markdown`] when
    /// it sees that flag. See [`Self::set_result_markdown`].
    result_markdown: Mutex<bool>,
    /// Complete todo snapshot, rendered as a single compact task list.
    todo_state: Mutex<Option<Vec<(String, String)>>>,
    plan_state: Mutex<Option<(String, bool)>>,
    /// Bumped by every mutator. A finished tool panel is immutable (no live
    /// elapsed timer), so its rendered lines are memoized by width; the
    /// revision drops the cache when args/result/expand change.
    revision: AtomicU64,
    /// Memoized render of a `Completed`/`Failed` panel. Mirrors upstream's
    /// per-component render cache (`tool-execution.ts` + `layout.ts
    /// renderCached`), which is what keeps a transcript repaint from
    /// re-rendering every finished tool panel.
    cache: Mutex<Option<ToolRenderCache>>,
}

#[derive(Clone)]
struct ToolRenderCache {
    width: usize,
    revision: u64,
    theme_revision: u64,
    lines: Arc<Vec<String>>,
}

impl ToolExecutionComponent {
    /// Create a new tool execution component.
    pub fn new(name: &str, args: &str) -> Self {
        Self {
            name: Mutex::new(name.to_string()),
            display_title: Mutex::new(None),
            skill_name: Mutex::new(None),
            args: Mutex::new(args.to_string()),
            result: Mutex::new(None),
            status: Mutex::new(ToolStatus::Pending),
            started_at: Mutex::new(None),
            finished_at: Mutex::new(None),
            expanded: Mutex::new(false),
            diff_lines: Mutex::new(None),
            result_markdown: Mutex::new(false),
            todo_state: Mutex::new(None),
            plan_state: Mutex::new(None),
            revision: AtomicU64::new(0),
            cache: Mutex::new(None),
        }
    }

    /// Invalidate the memoized render (state changed).
    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    /// Set the tool arguments.
    pub fn set_args(&self, args: &str) {
        if let Ok(mut a) = self.args.lock() {
            *a = args.to_string();
        }
        self.bump();
    }

    /// Set the tool result.
    pub fn set_result(&self, result: &str, is_error: bool) {
        if let Ok(mut r) = self.result.lock() {
            // The header glyph + bg tint already convey status (✗/red for
            // failure, ✓/green for success), so the result text is stored
            // raw — no redundant emoji prefix (pi doesn't double these up).
            *r = Some(result.to_string());
        }
        if let Ok(mut s) = self.status.lock() {
            *s = if is_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
        }
        if let Ok(mut f) = self.finished_at.lock() {
            *f = Some(Instant::now());
        }
        self.bump();
    }

    /// Mark as running.
    pub fn set_running(&self) {
        if let Ok(mut s) = self.status.lock() {
            *s = ToolStatus::Running;
        }
        if let Ok(mut t) = self.started_at.lock() {
            *t = Some(Instant::now());
        }
        self.bump();
    }

    /// Set expanded state.
    pub fn set_expanded(&self, expanded: bool) {
        if let Ok(mut e) = self.expanded.lock() {
            *e = expanded;
        }
        self.bump();
    }

    /// Whether the component is currently expanded (for toggle helpers).
    pub fn is_expanded(&self) -> bool {
        *self.expanded.lock().unwrap()
    }

    /// Get the status.
    pub fn status(&self) -> ToolStatus {
        *self.status.lock().unwrap()
    }

    /// Get the tool name.
    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    /// Override the title shown in the header without changing the underlying
    /// tool name used to summarize arguments and route updates.
    pub fn set_display_title(&self, title: impl Into<String>) {
        if let Ok(mut display_title) = self.display_title.lock() {
            *display_title = Some(title.into());
        }
        self.bump();
    }

    /// Mark this component as a skill-invocation read: it renders in the
    /// native-Pi `[skill] <name>` box style (custom-message background,
    /// collapsible to the full skill markdown). Pass the skill name.
    pub fn set_skill_name(&self, name: impl Into<String>) {
        if let Ok(mut skill_name) = self.skill_name.lock() {
            *skill_name = Some(name.into());
        }
        self.bump();
    }

    /// Whether this component displays a skill invocation (see
    /// [`Self::set_skill_name`]).
    pub fn is_skill(&self) -> bool {
        self.skill_name.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    /// The skill name when this component is a skill invocation.
    pub fn skill_name(&self) -> Option<String> {
        self.skill_name.lock().unwrap().clone()
    }

    /// Attach a pre-rendered colored diff (from [`crate::diff::render_diff`]).
    /// When set, the diff lines are always shown (regardless of `expanded`)
    /// so an edit's changes are visible directly in the transcript.
    pub fn set_diff(&self, lines: Vec<String>) {
        if let Ok(mut d) = self.diff_lines.lock() {
            *d = Some(lines);
        }
        self.bump();
    }

    /// Render the result body as markdown instead of plain indented text.
    ///
    /// The host turns this on when a tool result carries `details.markdown =
    /// true`, so a tool whose payload is markdown (a todo table, a report)
    /// gets its tables/lists/bold rendered by the shared markdown component
    /// rather than printed literally. Markdown rows already wrap themselves to
    /// the panel width, so they are not re-wrapped here.
    pub fn set_result_markdown(&self, enabled: bool) {
        if let Ok(mut markdown) = self.result_markdown.lock() {
            *markdown = enabled;
        }
        self.bump();
    }

    /// Install plain task rows; the host owns plugin snapshot parsing.
    pub fn set_todo_state(&self, rows: Vec<(String, String)>) {
        *self.todo_state.lock().unwrap() = Some(rows);
        self.bump();
    }

    pub fn is_todo_list(&self) -> bool {
        self.todo_state.lock().unwrap().is_some()
    }

    pub fn set_plan_state(&self, plan: String, active: bool) {
        *self.plan_state.lock().unwrap() = Some((plan, active));
        self.bump();
    }

    pub fn is_plan_panel(&self) -> bool {
        self.plan_state.lock().unwrap().is_some()
    }
}

impl ToolExecutionComponent {
    /// Render one theme snapshot, independently of the production render cache.
    fn render_panel(&self, width: usize, theme: &Theme) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let colors = &theme.colors;
        if let Some((plan, active)) = self.plan_state.lock().unwrap().as_ref() {
            let status = if *active {
                "Planning"
            } else if plan.trim().is_empty() {
                "Idle"
            } else {
                "Ready"
            };
            let mut rows = vec![truncate_to_width(
                &colors.accent.fg(&bold(&format!(" Plan · {status}"))),
                width,
                "…",
            )];
            if plan.trim().is_empty() {
                let hint = if *active {
                    "Inspect the repository, then complete the plan."
                } else {
                    "No saved plan. Use /plan start."
                };
                for line in wrap_text_with_ansi(hint, width.saturating_sub(2).max(1)) {
                    rows.push(truncate_to_width(
                        &format!("  {}", colors.muted.fg(&line)),
                        width,
                        "…",
                    ));
                }
            } else {
                let content = crate::markdown::Markdown::new(plan.clone(), 0, 0)
                    .render(width.saturating_sub(2).max(1));
                let expanded = *self.expanded.lock().unwrap();
                let shown = if expanded {
                    content.len()
                } else {
                    content.len().min(16)
                };
                for line in content.iter().take(shown) {
                    rows.push(truncate_to_width(&format!("  {line}"), width, "…"));
                }
                if content.len() > shown {
                    rows.push(truncate_to_width(
                        &colors.muted.fg(&format!(
                            "  … {} more lines · Ctrl+O or /plan show",
                            content.len() - shown
                        )),
                        width,
                        "…",
                    ));
                }
            }
            // Trailing spacer (one convention for every panel: each transcript
            // entry ends with its own blank row; nothing adds a leading one).
            rows.push(String::new());
            return rows;
        }
        if let Some(todos) = self.todo_state.lock().unwrap().as_ref() {
            let done = todos
                .iter()
                .filter(|(_, status)| status == "completed")
                .count();
            let heading = format!(" Tasks  {done}/{} completed", todos.len());
            let mut rows = vec![truncate_to_width(
                &colors.tool_title.fg(&bold(&heading)),
                width,
                "…",
            )];
            if todos.is_empty() {
                rows.push(colors.muted.fg("   No tasks"));
            }
            for (text, status) in todos {
                let (mark, color) = match status.as_str() {
                    "completed" => ("[✓]", colors.muted),
                    "in_progress" => ("[•]", colors.accent),
                    _ => ("[ ]", colors.tool_output),
                };
                for (index, line) in wrap_text_with_ansi(text, width.saturating_sub(7).max(1))
                    .into_iter()
                    .enumerate()
                {
                    let label = if status == "completed" {
                        format!("\x1b[9m{line}\x1b[29m")
                    } else {
                        line
                    };
                    let prefix = if index == 0 { mark } else { "   " };
                    rows.push(truncate_to_width(
                        &format!("   {} {}", color.fg(prefix), color.fg(&label)),
                        width,
                        "…",
                    ));
                }
            }
            // Trailing spacer (one convention for every panel; see the plan
            // arm above).
            rows.push(String::new());
            return rows;
        }
        let mut lines = Vec::new();

        let name = self.name.lock().unwrap();
        let display_title = self.display_title.lock().unwrap();
        let skill_name = self.skill_name.lock().unwrap();
        let status = self.status.lock().unwrap();
        let args = self.args.lock().unwrap();
        let result = self.result.lock().unwrap();
        let expanded = self.expanded.lock().unwrap();
        let diff_lines = self.diff_lines.lock().unwrap();
        let result_markdown = self.result_markdown.lock().unwrap();
        let started_at = self.started_at.lock().unwrap();
        let finished_at = self.finished_at.lock().unwrap();

        // Skill invocation mode: render as native-Pi's `[skill]` box rather
        // than a generic tool panel — custom-message background, collapsed to
        // a single `[skill] <name> (Ctrl+O to expand)` line, expanded to the
        // full skill markdown (the `read` result). Mirrors pi's
        // `SkillInvocationMessageComponent` while reusing the tool panel's
        // result streaming + Ctrl+O expand plumbing.
        if let Some(skill) = skill_name.as_deref() {
            let content = result.as_deref().unwrap_or("");
            let mut lines = render_skill(colors, skill, content, *expanded, width);
            // Trailing spacer (one convention for every panel). The skill box
            // used to skip the top spacer entirely — its render path
            // early-returned past the push — so it had no gap on either side.
            lines.push(String::new());
            return lines;
        }

        // Status indicator — a colored glyph (no emoji), pi style.
        let (status_icon, status_color) = match *status {
            ToolStatus::Pending => ("●", colors.muted),
            ToolStatus::Running => ("●", colors.accent),
            ToolStatus::Completed => ("✓", colors.success),
            ToolStatus::Failed => ("✗", colors.error),
        };

        // Background tint for the header row reflects status (pi tool*Bg).
        let bg = match *status {
            ToolStatus::Pending | ToolStatus::Running => colors.tool_pending_bg,
            ToolStatus::Failed => colors.tool_error_bg,
            ToolStatus::Completed => colors.tool_success_bg,
        };
        let panel = ToolPanel::new(width, theme.spacing, bg).expect("nonzero panel width");
        let content_width = panel.content_width;
        lines.extend(std::iter::repeat(panel.blank()).take(panel.padding_y));

        // Tool header line.
        // For args JSON we display a compact signature (e.g. `read src/foo.rs`)
        // instead of the raw `{"path":"..."}` blob — see `parse_args_summary`.
        // The verbose JSON still shows in the expanded args block below.
        let chevron = if *expanded { "▾" } else { "▸" };
        let label = display_title
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| tools::tool_label(&name));
        let summary = tools::parse_args_summary(&name, &args);
        // Calculate elapsed time
        let elapsed_str = if *status == ToolStatus::Running {
            // Regular-mode transcript headers must stay stable when they enter
            // saved scrollback. The editor/status dock still reports live time;
            // fullscreen panels keep their per-tool clock as before.
            if crate::render::is_main_screen_frame() {
                String::new()
            } else if let Some(started) = *started_at {
                let elapsed = started.elapsed();
                format_elapsed(elapsed)
            } else {
                String::new()
            }
        } else if *status == ToolStatus::Completed || *status == ToolStatus::Failed {
            // Total elapsed after completion
            if let (Some(started), Some(finished)) = (*started_at, *finished_at) {
                let elapsed = finished.duration_since(started);
                format_elapsed(elapsed)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let head_parts = if summary.is_empty() && elapsed_str.is_empty() {
            format!(
                "{} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.muted.fg(chevron),
            )
        } else if summary.is_empty() {
            format!(
                "{} {} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.muted.fg(&elapsed_str),
                colors.muted.fg(chevron),
            )
        } else if elapsed_str.is_empty() {
            format!(
                "{} {} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.tool_output.fg(&summary),
                colors.muted.fg(chevron),
            )
        } else {
            format!(
                "{} {} {} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.tool_output.fg(&summary),
                colors.muted.fg(&elapsed_str),
                colors.muted.fg(chevron),
            )
        };
        // Do not sacrifice a long tool name/path summary for the running
        // indicator. Tool calls often carry generated paths or extension
        // names; wrap the header into panel rows so it remains inspectable.
        for part in wrap_text_with_ansi(&head_parts, content_width) {
            lines.push(panel.line(&part));
        }

        // Expanded: the full args JSON (indented, dim) above the result. The
        // compact summary already lives in the header, so this is the
        // "show me everything" path for debugging a tool call.
        if *expanded && !args.is_empty() && args.trim() != "{}" {
            let args_line = format!(
                "{} {}",
                colors.muted.fg("args:"),
                colors.dim.fg(&pretty_args(&args))
            );
            for part in wrap_text_with_ansi(&args_line, content_width) {
                lines.push(panel.line(&part));
            }
        }

        // Show result if available (always — a failure's error text is the
        // payload, not a detail to hide behind expand). Long logical lines wrap
        // inside the panel instead of overflowing into the next terminal row.
        if let Some(ref r) = *result {
            // The gutter belongs to the content, not the panel's outer inset.
            // Drop it only when it would leave no room for output text.
            let gutter = !*result_markdown && content_width > 2;
            let body_width = content_width - if gutter { 2 } else { 0 };
            // Markdown results are rendered by the markdown component, which
            // wraps and styles its own rows (tables, lists, inline code). The
            // plain path keeps the existing naive wrap so non-markdown tool
            // output is unchanged.
            let visual_lines: Vec<String> = if *result_markdown {
                crate::markdown::Markdown::new(r.clone(), 0, 0).render(body_width)
            } else {
                normalized_output_lines(r)
                    .into_iter()
                    .flat_map(|line| wrap_text_with_ansi(&line, body_width))
                    .collect()
            };
            let shown = if *expanded {
                visual_lines.len()
            } else {
                visual_lines.len().min(OUTPUT_PREVIEW_LINES)
            };
            for part in visual_lines.iter().take(shown) {
                let result_line = if gutter {
                    format!("{} {}", colors.dim.fg("│"), colors.tool_output.fg(part))
                } else if *result_markdown {
                    // Markdown carries its own styling, including table edges.
                    part.clone()
                } else {
                    colors.tool_output.fg(part)
                };
                lines.push(panel.line(&result_line));
            }
            let hidden = visual_lines.len().saturating_sub(shown);
            if hidden > 0 {
                let hint = colors
                    .muted
                    .fg(&format!("… {hidden} more lines (Ctrl+O to expand)"));
                for part in wrap_text_with_ansi(&hint, content_width) {
                    lines.push(panel.line(&part));
                }
            } else if *expanded && visual_lines.len() > OUTPUT_PREVIEW_LINES {
                let hint = colors.muted.fg("Ctrl+O to collapse");
                for part in wrap_text_with_ansi(&hint, content_width) {
                    lines.push(panel.line(&part));
                }
            }
        }

        // A colored diff (from `render_diff`) is shown collapsed by default
        // (a small preview) and fully when expanded — an edit's diff is the
        // useful payload, but dumping 40 lines into the transcript by default
        // swamped the conversation. Cap at DIFF_LINE_CAP even when expanded.
        if let Some(diff) = diff_lines.as_ref() {
            let total = diff.len();
            let cap = if *expanded {
                DIFF_LINE_CAP
            } else {
                DIFF_PREVIEW_LINES
            };
            let shown = diff.iter().take(cap);
            for dl in shown {
                // Keep the diff's fixed gutter and clipping behavior, but put
                // every row (including blanks) inside the themed panel surface.
                lines.push(panel.line(&truncate_to_width(dl, content_width, "…")));
            }
            if total > cap {
                let more = total - cap;
                let hint = if *expanded {
                    format!("… {} more diff lines (Ctrl+O to collapse)", more)
                } else {
                    format!("… {} more diff lines (Ctrl+O to expand)", more)
                };
                for part in wrap_text_with_ansi(&colors.muted.fg(&hint), content_width) {
                    lines.push(panel.line(&part));
                }
            }
        }

        lines.extend(std::iter::repeat(panel.blank()).take(panel.padding_y));
        // Trailing spacer (one convention for every panel: each transcript
        // entry ends with its own blank row; nothing adds a leading one).
        lines.push(String::new());
        lines
    }
}

impl ToolExecutionComponent {
    fn shared_lines(&self, width: usize) -> Arc<Vec<String>> {
        let status = *self.status.lock().unwrap();
        let revision = self.revision.load(Ordering::Acquire);
        let theme_revision = theme_revision();

        // Only finished panels are immutable: running ones have a live timer.
        let finished = matches!(status, ToolStatus::Completed | ToolStatus::Failed);
        if finished {
            if let Ok(cache) = self.cache.lock() {
                if let Some(cached) = cache.as_ref() {
                    if cached.width == width
                        && cached.revision == revision
                        && cached.theme_revision == theme_revision
                    {
                        return cached.lines.clone();
                    }
                }
            }
        }
        let lines = Arc::new(self.render_panel(width, &theme()));
        if finished {
            if let Ok(mut cache) = self.cache.lock() {
                *cache = Some(ToolRenderCache {
                    width,
                    revision,
                    theme_revision,
                    lines: lines.clone(),
                });
            }
        } else if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
        lines
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.shared_lines(width).as_ref().clone()
    }

    fn render_document(&self, width: usize) -> RenderedDocument {
        RenderedDocument::from_shared(self.shared_lines(width))
    }

    fn invalidate(&self) {
        self.bump();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Render a skill-invocation box in native-Pi style.
///
/// Mirrors pi's `SkillInvocationMessageComponent`: a `[skill]` box on the
/// custom-message background. Collapsed it is a single line
/// `[skill] <name> (Ctrl+O to expand)`; expanded it shows the `[skill]` label,
/// the bold skill name, then the full skill markdown content (the `read`
/// result) rendered with the standard markdown component.
fn render_skill(
    colors: &ThemeColors,
    name: &str,
    content: &str,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    let bg = colors.custom_message_bg;
    let label = crate::ansi::bold("[skill]");
    // Upstream's `Box(1, 1, bg)` — one column of horizontal padding on each
    // side, one blank background row top and bottom.
    let pad = " ";
    let mut out: Vec<String> = Vec::new();

    // Box top padding (pi Box(1,1)).
    out.push(apply_background_to_line("", width, |s| bg.bg(s)));

    if !expanded {
        // Single collapsed line: `[skill] <name> (Ctrl+O to expand)`.
        let line = format!(
            "{pad}{} {} {}",
            colors.custom_message_label.fg(&label),
            colors.custom_message_text.fg(name),
            colors.dim.fg("(Ctrl+O to expand)"),
        );
        out.push(apply_background_to_line(&line, width, |s| bg.bg(s)));
        out.push(apply_background_to_line("", width, |s| bg.bg(s)));
        return out;
    }

    // Expanded: `[skill]` label, then a single markdown block whose first
    // paragraph is the bold skill name (upstream renders
    // `**${name}**\n\n${content}` as one Markdown child), so the body keeps
    // its normal markdown colors on the custom-message background.
    out.push(apply_background_to_line(
        &format!("{pad}{}", colors.custom_message_label.fg(&label)),
        width,
        |s| bg.bg(s),
    ));

    let header = format!("{pad}{}", colors.custom_message_text.fg(&bold(name)));
    out.push(apply_background_to_line(&header, width, |s| bg.bg(s)));
    out.push(apply_background_to_line("", width, |s| bg.bg(s)));

    let body = strip_frontmatter(content);
    if !body.is_empty() {
        // Render the skill body with the shared markdown component. The
        // markdown component handles code blocks / tables / headings; each
        // line is re-tinted onto the custom-message background so the box
        // reads as a single panel.
        let body_width = width.saturating_sub(2).max(1);
        let md = crate::markdown::Markdown::new(body.to_string(), 0, 0);
        for md_line in md.render(body_width) {
            let line = format!("{pad}{md_line}");
            out.push(apply_background_to_line(&line, width, |s| bg.bg(s)));
        }
    }

    // Box bottom padding (pi Box(1,1)).
    out.push(apply_background_to_line("", width, |s| bg.bg(s)));
    out
}

/// Strip a leading YAML frontmatter block (`---\n...\n---`) from skill file
/// content. Upstream's skill block carries the frontmatter-stripped body
/// (`parseSkillBlock` reads the `<skill>` payload, not the raw file), so the
/// expanded box shows the same instructions the model received.
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    let Some(end) = rest.find("\n---") else {
        return content;
    };
    let after = &rest[end + 4..];
    // Drop the blank line the closing delimiter is conventionally followed by
    // so the markdown body starts on its first heading/paragraph.
    after.strip_prefix('\n').unwrap_or(after)
}

/// Format a duration as a human-readable elapsed time string.
/// Shows seconds with one decimal place for short durations,
/// or minutes and seconds for longer durations.
pub(crate) fn format_elapsed(duration: std::time::Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let remaining_secs = (secs % 60.0).floor() as u64;
        format!("{}m{}s", mins, remaining_secs)
    }
}

/// Normalize tool output for display. Tool payloads are not guaranteed to use
/// Unix line endings and may contain tabs/control sequences from subprocesses.
fn normalized_output_lines(output: &str) -> Vec<String> {
    let clean = strip_ansi(output).replace("\r\n", "\n").replace('\r', "\n");
    clean
        .split('\n')
        .map(|line| {
            let expanded = line.replace('\t', "    ");
            expanded
                .chars()
                .filter(|c| *c == ' ' || !c.is_control())
                .collect()
        })
        .collect()
}

/// Best-effort pretty-print of a tool args JSON blob for the expanded args
/// block. Falls back to the raw string if it isn't valid JSON. Kept inline
/// rather than reaching for `serde_json::to_string_pretty` so `rpi-tui` stays
/// free of a serde dep (project constraint) — this is display-only and a
/// compact one-liner is more useful in a terminal than a multi-line dump.
fn pretty_args(args: &str) -> String {
    // Compact single-line: `{"path":"src/foo.rs","offset":1}` →
    // `path=src/foo.rs offset=1`. Strings drop quotes, numbers/bools as-is.
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return "{}".to_string();
    }
    // Lightweight parse: must start with `{`. If not, show raw.
    if !trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    let inner = tools::flatten_json_object(trimmed);
    if inner.is_empty() {
        trimmed.to_string()
    } else {
        inner
    }
}

/// Tool-display helpers: turn a raw tool name + args JSON into a compact,
/// human-readable signature for the tool-block header (e.g.
/// `read src/foo.rs`, `grep pattern · src`, `bash ls -la`).
///
/// Lives in `rpi-tui` (no `rpi-tools` dep) so the render crate can stay
/// standalone; the mapping is purely lexical over the known builtin tool
/// arg shapes. Unknown tools fall back to the raw args.
mod tools {
    /// A short label for the tool name in the header. Pi displays tool
    /// names in their natural lowercase form (`read`, `edit`, …) with bold
    /// styling, matching the original casing from the tool definition.
    pub fn tool_label(name: &str) -> String {
        name.to_string()
    }

    /// Parse a known builtin tool's args JSON into a compact arg summary.
    /// Returns "" when there's nothing useful to show (e.g. unknown tool or
    /// empty args) so the caller can fall back to a header with no summary.
    ///
    /// Handles both complete and incomplete (streaming) JSON — when the args
    /// are still streaming, we fall back to a simple string search for the
    /// key fields so the header can show the file path even before the JSON
    /// is fully received.
    pub fn parse_args_summary(tool: &str, args_json: &str) -> String {
        let map = match parse_json_object(args_json) {
            Some(m) => m,
            None => {
                // JSON is incomplete (streaming) — fall back to simple
                // string extraction for the key field.
                return extract_incomplete_args(tool, args_json);
            }
        };
        let get = |k: &str| field(&map, k);
        match tool {
            "read" => get("path"),
            "write" => get("path"),
            "ls" => get("path"),
            "find" => {
                let p = get("pattern");
                let dir = get("path");
                match dir.as_str() {
                    "" => p,
                    d => format!("{} · {}", p, d),
                }
            }
            "grep" => {
                let p = get("pattern");
                let path = get("path");
                let glob = get("glob");
                let mut parts = vec![p];
                if !glob.is_empty() {
                    parts.push(format!("glob {}", glob));
                }
                if !path.is_empty() {
                    parts.push(path);
                }
                parts.join(" · ")
            }
            "edit" => {
                // path + count of edits (oldText is too long to inline).
                let path = get("path");
                if let Some(arr) = parse_json_array_len(args_json, "edits") {
                    if arr == 1 {
                        path
                    } else {
                        format!("{} ({} edits)", path, arr)
                    }
                } else {
                    path
                }
            }
            "bash" => get("command"),
            _ => {
                // Unknown tool: show the first string-valued field if any.
                map.into_iter().next().map(|(_, v)| v).unwrap_or_default()
            }
        }
    }

    /// Extract a summary from incomplete/streaming JSON by simple string matching.
    /// This is a fallback when the JSON is not yet complete (streaming in progress).
    /// We use regex-like pattern matching to extract key fields like "path".
    fn extract_incomplete_args(tool: &str, args_json: &str) -> String {
        match tool {
            "read" | "write" | "edit" | "ls" => extract_string_field(args_json, "path"),
            "find" => {
                let pattern = extract_string_field(args_json, "pattern");
                let path = extract_string_field(args_json, "path");
                if path.is_empty() {
                    pattern
                } else if pattern.is_empty() {
                    path
                } else {
                    format!("{} · {}", pattern, path)
                }
            }
            "grep" => {
                let pattern = extract_string_field(args_json, "pattern");
                let path = extract_string_field(args_json, "path");
                let glob = extract_string_field(args_json, "glob");
                let mut parts = vec![pattern];
                if !glob.is_empty() {
                    parts.push(format!("glob {}", glob));
                }
                if !path.is_empty() {
                    parts.push(path);
                }
                parts.join(" · ")
            }
            "bash" => extract_string_field(args_json, "command"),
            _ => extract_any_string_field(args_json),
        }
    }

    /// Extract a string field value from JSON using simple pattern matching.
    /// Looks for patterns like `"key":"value"` or `"key": "value"`.
    /// This is a best-effort extraction for incomplete/streaming JSON.
    fn extract_string_field(json: &str, key: &str) -> String {
        // Pattern 1: "key":"value" (no space)
        let pattern1 = format!("\"{}\":\"", key);
        if let Some(start) = json.find(&pattern1) {
            let value_start = start + pattern1.len();
            let remaining = &json[value_start..];
            if let Some(end) = remaining.find('"') {
                let value = &remaining[..end];
                // Unescape common JSON escapes
                return value
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\")
                    .replace("\\n", "\n")
                    .replace("\\t", "\t");
            }
        }

        // Pattern 2: "key": "value" (with space)
        let pattern2 = format!("\"{}\": \"", key);
        if let Some(start) = json.find(&pattern2) {
            let value_start = start + pattern2.len();
            let remaining = &json[value_start..];
            if let Some(end) = remaining.find('"') {
                let value = &remaining[..end];
                // Unescape common JSON escapes
                return value
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\")
                    .replace("\\n", "\n")
                    .replace("\\t", "\t");
            }
        }

        String::new()
    }

    /// Extract any string field value from incomplete JSON.
    /// Used as a fallback for unknown tools.
    fn extract_any_string_field(json: &str) -> String {
        // Look for the first string value pattern: "something":"value"
        let pattern = "\":\"";
        if let Some(start) = json.find(pattern) {
            let value_start = start + pattern.len();
            let remaining = &json[value_start..];
            if let Some(end) = remaining.find('"') {
                let value = &remaining[..end];
                // Unescape common JSON escapes
                return value
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\")
                    .replace("\\n", "\n")
                    .replace("\\t", "\t");
            }
        }
        String::new()
    }

    /// Look up a field's value (unquoted) in a parsed object map.
    fn field(map: &[(String, String)], key: &str) -> String {
        map.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| unquote(v))
            .unwrap_or_default()
    }

    /// Flatten a JSON object string into `k=v k=v` (strings unquoted).
    /// Returns "" if the input isn't a `{...}` object or has no fields.
    /// This is a display-only best-effort parser — it does NOT handle nested
    /// objects/arrays (those stay as their raw JSON substring) or escapes;
    /// it's good enough for the expanded args block where the compact header
    /// already showed the important bit.
    pub fn flatten_json_object(json: &str) -> String {
        let Some(map) = parse_json_object(json) else {
            return String::new();
        };
        map.into_iter()
            .map(|(k, v)| format!("{}={}", k, unquote(&v)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Parse a flat JSON object `{"k":"v",...}` into an ordered (k,v) list.
    /// Values are kept as their raw JSON substring (strings keep quotes;
    /// numbers/bools are bare). Returns None if the input isn't a flat object.
    ///
    /// This is a tiny hand-rolled scanner — `rpi-tui` deliberately has no
    /// serde dep. It handles the arg shapes the builtin tools emit (flat,
    /// string/number/bool values); nested objects/arrays are returned as
    /// their raw substring (the caller treats them opaquely).
    fn parse_json_object(json: &str) -> Option<Vec<(String, String)>> {
        let s = json.trim();
        if !s.starts_with('{') || !s.ends_with('}') {
            return None;
        }
        let inner = &s[1..s.len() - 1];
        let mut out = Vec::new();
        let mut chars = inner.chars().peekable();
        loop {
            skip_ws(&mut chars);
            if chars.peek().is_none() {
                break;
            }
            // key (must be a quoted string)
            if chars.peek() != Some(&'"') {
                return None;
            }
            let key = read_string(&mut chars)?;
            skip_ws(&mut chars);
            if chars.next() != Some(':') {
                return None;
            }
            skip_ws(&mut chars);
            // value: string, number, true/false/null, or nested (raw).
            let val = read_value(&mut chars);
            out.push((key, val));
            skip_ws(&mut chars);
            match chars.peek() {
                Some(&',') => {
                    chars.next();
                }
                Some(_) => {
                    return None;
                } // malformed
                None => break,
            }
        }
        Some(out)
    }

    /// Count the elements of a JSON array field `name` in `json`. Used by the
    /// edit summary to show `(N edits)` without holding the array contents.
    fn parse_json_array_len(json: &str, name: &str) -> Option<usize> {
        // Find `"name":` then the `[...]` that follows.
        let pat = format!("\"{}\":", name);
        let idx = json.find(&pat)?;
        let rest = &json[idx + pat.len()..];
        let rest = rest.trim_start();
        if !rest.starts_with('[') {
            return None;
        }
        // Scan the array body respecting nested brackets + strings.
        let mut depth = 0isize;
        let mut in_str = false;
        let mut esc = false;
        let mut count = 0usize;
        let mut saw_any = false;
        for c in rest.chars() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                ',' if depth == 1 => count += 1,
                _ if !c.is_whitespace() && depth >= 1 => saw_any = true,
                _ => {}
            }
        }
        if !saw_any {
            Some(0)
        } else {
            Some(count + 1)
        }
    }

    fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars>) {
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
    }

    fn read_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
        if chars.next()? != '"' {
            return None;
        }
        let mut s = String::new();
        let mut esc = false;
        for c in chars.by_ref() {
            if esc {
                esc = false;
                s.push(c);
                continue;
            }
            match c {
                '\\' => esc = true,
                '"' => return Some(s),
                _ => s.push(c),
            }
        }
        None
    }

    fn read_value(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
        match chars.peek() {
            Some(&'"') => {
                // Quoted string — include the quotes in the raw value (so the
                // caller can distinguish strings from numbers); `unquote`
                // strips them for display.
                let mut s = String::from("\"");
                chars.next();
                let mut esc = false;
                for c in chars.by_ref() {
                    s.push(c);
                    if esc {
                        esc = false;
                        continue;
                    }
                    if c == '\\' {
                        esc = true;
                        continue;
                    }
                    if c == '"' {
                        break;
                    }
                }
                s
            }
            Some(&'[') | Some(&'{') => read_nested(chars),
            _ => {
                // Bare token (number/true/false/null) — read until , or }.
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ',' || c == '}' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                s.trim().to_string()
            }
        }
    }

    fn read_nested(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
        // Copy the raw substring for a nested array/object, tracking depth +
        // string state so commas inside don't terminate it early.
        let mut s = String::new();
        let mut depth = 0isize;
        let mut in_str = false;
        let mut esc = false;
        for c in chars.by_ref() {
            s.push(c);
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        s
    }

    fn unquote(v: &str) -> String {
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v[1..v.len() - 1].to_string()
        } else {
            v.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn regular_panels_have_stable_running_headers_and_keep_final_elapsed() {
        use super::*;
        use std::time::Duration;

        let tool = ToolExecutionComponent::new("read", r#"{"path":"complete-path.rs"}"#);
        tool.set_running();
        *tool.started_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(2));
        let fullscreen = tool.render(80);
        let regular = crate::render::with_main_screen_frame(|| tool.render(80));
        assert!(strip_ansi(&regular.join("\n")).contains("complete-path.rs"));
        assert_ne!(regular, fullscreen, "fullscreen must keep its live timer");

        *tool.started_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(65));
        assert_ne!(tool.render(80), fullscreen, "fullscreen clock must advance");
        assert_eq!(
            crate::render::with_main_screen_frame(|| tool.render(80)),
            regular,
            "regular transcript must not change with wall-clock time"
        );

        tool.set_result("complete result", false);
        let start = Instant::now() - Duration::from_secs(10);
        *tool.started_at.lock().unwrap() = Some(start);
        *tool.finished_at.lock().unwrap() = Some(start + Duration::from_millis(3200));
        let finished_regular = crate::render::with_main_screen_frame(|| tool.render(80));
        assert!(strip_ansi(&finished_regular.join("\n")).contains("3.2s"));
        assert_eq!(finished_regular, tool.render(80));
    }

    #[test]
    fn finalized_documents_share_rows_and_refresh_after_changes() {
        use super::*;
        for failed in [false, true] {
            let tool = ToolExecutionComponent::new("read", "{}");
            let output = (1..=20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            tool.set_result(&output, failed);
            let first = tool.shared_lines(80);
            assert!(Arc::ptr_eq(&first, &tool.shared_lines(80)));
            let references = Arc::strong_count(&first);
            let document = tool.render_document(80);
            let second_document = tool.render_document(80);
            assert_eq!(Arc::strong_count(&first), references + 2);
            assert_eq!(document.lines(0, document.len()), first.as_ref().clone());
            assert_eq!(second_document.len(), document.len());
            assert_eq!(tool.render(80), first.as_ref().clone());

            tool.set_expanded(true);
            let expanded = tool.shared_lines(80);
            assert!(!Arc::ptr_eq(&first, &expanded));
            assert!(strip_ansi(&expanded.join("\n")).contains("line 20"));
            assert!(!strip_ansi(&first.join("\n")).contains("line 20"));

            tool.set_result("late result", failed);
            let updated = tool.shared_lines(80);
            assert!(!Arc::ptr_eq(&expanded, &updated));
            assert!(strip_ansi(&updated.join("\n")).contains("late result"));
            let narrow = tool.shared_lines(24);
            assert!(!Arc::ptr_eq(&updated, &narrow));
            assert!(narrow
                .iter()
                .all(|row| crate::utils::visible_width(row) <= 24));
            tool.invalidate();
            let invalidated = tool.shared_lines(24);
            assert!(!Arc::ptr_eq(&narrow, &invalidated));
            assert_eq!(narrow, invalidated);
            assert!(Arc::ptr_eq(&invalidated, &tool.shared_lines(24)));
        }
    }

    #[test]
    fn pending_and_running_rows_remain_live_and_uncached() {
        use super::*;
        let tool = ToolExecutionComponent::new("read", "{}");
        let pending = tool.shared_lines(80);
        assert!(!Arc::ptr_eq(&pending, &tool.shared_lines(80)));
        assert!(tool.cache.lock().unwrap().is_none());
        tool.set_result("finished", false);
        let finished = tool.shared_lines(80);
        tool.set_running();
        let running = tool.shared_lines(80);
        assert!(!Arc::ptr_eq(&finished, &running));
        assert!(!Arc::ptr_eq(&running, &tool.shared_lines(80)));
        assert!(tool.cache.lock().unwrap().is_none());
        // Move the clock origin directly rather than sleeping or relying on timing.
        *tool.started_at.lock().unwrap() =
            Some(Instant::now() - std::time::Duration::from_secs(65));
        let later = tool.shared_lines(80);
        assert!(strip_ansi(&later.join("\n")).contains("1m5s"));
        assert!(!Arc::ptr_eq(&running, &later));
    }

    #[test]
    fn plan_panel_keeps_markdown_and_hides_raw_arguments() {
        use super::*;
        let panel = ToolExecutionComponent::new("plan_mode_complete", "RAW_PLAN_ARGUMENT");
        panel.set_result("Plan complete. Normal tool access restored.", false);
        panel.set_plan_state(
            "## Implementation\n\n1. Inspect code\n2. Update UI\n\n## Validation\n\nRun tests"
                .into(),
            false,
        );
        panel.set_expanded(true);
        let rendered = strip_ansi(&panel.render(60).join("\n"));
        assert!(rendered.contains("Plan · Ready"));
        assert!(rendered.contains("Implementation"));
        assert!(rendered.contains("Validation"));
        assert!(rendered.contains("Inspect code"));
        assert!(rendered.contains("Update UI"));
        assert!(!rendered.contains("RAW_PLAN_ARGUMENT"));
        assert!(!rendered.contains("args:"));
        assert!(!rendered.contains("Normal tool access"));
        panel.set_plan_state(String::new(), false);
        assert!(strip_ansi(&panel.render(60).join("\n")).contains("Plan · Idle"));
    }

    #[test]
    fn completed_plan_can_expand_to_the_full_body() {
        use super::*;
        let panel = ToolExecutionComponent::new("plan_mode_complete", "{}");
        panel.set_result("", false);
        panel.set_plan_state(
            (1..=30)
                .map(|i| format!("{i}. Step {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        );
        let preview = strip_ansi(&panel.render(60).join("\n"));
        assert!(preview.contains("more lines"));
        assert!(!preview.contains("Step 30"));
        panel.set_expanded(true);
        let full = strip_ansi(&panel.render(60).join("\n"));
        assert!(full.contains("Step 30"));
    }
    #[test]
    fn task_list_keeps_crlf_rows_distinct() {
        use super::*;
        // CRLF segments as one grapheme; the shared wrapper dropped the pair
        // as controls and glued the rows into one task ("firstsecond").
        let panel = ToolExecutionComponent::new("todo", "{}");
        panel.set_todo_state(vec![("first\r\nsecond".into(), "pending".into())]);
        let rows = panel.render(80);
        let rendered = strip_ansi(&rows.join("\n"));
        assert!(
            rendered.contains("first") && rendered.contains("second"),
            "CRLF task rows lost: {rendered}"
        );
        let first_line = rows
            .iter()
            .enumerate()
            .find(|(_, l)| strip_ansi(l).contains("first"))
            .map(|(i, _)| i);
        let second_line = rows
            .iter()
            .enumerate()
            .find(|(_, l)| strip_ansi(l).contains("second"))
            .map(|(i, _)| i);
        assert_ne!(
            first_line, second_line,
            "CRLF must render as two distinct rows: {rendered}"
        );
    }

    #[test]
    fn task_list_wraps_and_distinguishes_statuses() {
        use super::*;
        let panel = ToolExecutionComponent::new("todo", "{}");
        panel.set_result("ignored summary", false);
        panel.set_todo_state(vec![
            ("Finished task".into(), "completed".into()),
            ("正在处理一个很长的任务名称".into(), "in_progress".into()),
            ("Next task".into(), "pending".into()),
        ]);
        let rows = panel.render(24);
        let rendered = strip_ansi(&rows.join("\n"));
        assert!(rendered.contains("1/3 completed"));
        assert!(rendered.contains("[✓] Finished task"));
        assert!(rendered.contains("[•]"));
        assert!(rendered.contains("[ ] Next task"));
        assert!(rows.iter().any(|line| line.contains("\x1b[9m")));
        assert!(!rendered.contains("ignored summary"));
        assert!(!rendered.contains("args:"));
        for line in rows {
            assert!(crate::utils::visible_width(&line) <= 24);
        }
    }
    use super::*;

    #[test]
    fn panels_carry_a_trailing_spacer_and_no_leading_one() {
        // Trailing-spacing convention: every transcript entry contributes one
        // blank line AFTER itself (the host adds none), so two adjacent
        // blocks are separated by exactly one blank regardless of which kind
        // sits on top. A leading spacer would double up with the previous
        // entry's trailing one; a missing trailing one leaves the next
        // message flush against the panel.
        let theme = Theme::default();
        for (panel, header) in [
            (
                {
                    let tool = ToolExecutionComponent::new("read", r#"{"path":"f.txt"}"#);
                    tool.set_result("done", false);
                    tool
                },
                "read",
            ),
            (
                {
                    let plan = ToolExecutionComponent::new("plan_mode_complete", "");
                    plan.set_plan_state("step 1".into(), false);
                    plan
                },
                "Plan",
            ),
            (
                {
                    let todo = ToolExecutionComponent::new("todo", "");
                    todo.set_todo_state(vec![("task".into(), "pending".into())]);
                    todo
                },
                "Tasks",
            ),
        ] {
            let lines = panel.render_panel(40, &theme);
            let plain: Vec<_> = lines.iter().map(|l| strip_ansi(l)).collect();
            assert!(plain.iter().any(|l| l.contains(header)));
            assert_eq!(
                lines.last(),
                Some(&String::new()),
                "{header} panel must end with the trailing spacer: {plain:?}"
            );
            assert_ne!(
                lines.first(),
                Some(&String::new()),
                "{header} panel must not open with a spacer: {plain:?}"
            );
        }
        // The skill box used to discard the top spacer entirely (its render
        // path early-returns past the spacer push), so it had no gap on
        // either side.
        let skill = ToolExecutionComponent::new("read", r#"{"path":"skills/foo/SKILL.md"}"#);
        skill.set_skill_name("foo");
        skill.set_result("instructions", false);
        let lines = skill.render_panel(40, &theme);
        assert_eq!(
            lines.last(),
            Some(&String::new()),
            "skill box must end with the trailing spacer: {lines:?}"
        );
    }

    #[test]
    fn generic_panels_share_themed_insets_in_every_state() {
        let theme = Theme::default();
        for status in [
            ToolStatus::Pending,
            ToolStatus::Running,
            ToolStatus::Completed,
            ToolStatus::Failed,
        ] {
            let tool = ToolExecutionComponent::new("edit", r#"{"path":"f.rs"}"#);
            tool.set_result("updated", status == ToolStatus::Failed);
            tool.set_diff(vec![
                theme.colors.tool_diff_added.fg("+ new"),
                String::new(),
            ]);
            *tool.status.lock().unwrap() = status;
            let bg = match status {
                ToolStatus::Pending | ToolStatus::Running => theme.colors.tool_pending_bg,
                ToolStatus::Completed => theme.colors.tool_success_bg,
                ToolStatus::Failed => theme.colors.tool_error_bg,
            };
            let lines = tool.render_panel(40, &theme);
            let blank = bg.bg(&" ".repeat(40));
            assert_eq!(lines[0], blank);
            assert_eq!(
                lines.last().unwrap(),
                &String::new(),
                "trailing spacer must remain uncolored"
            );
            assert!(strip_ansi(&lines[1]).starts_with("  "));
            assert!(lines
                .iter()
                .any(|line| strip_ansi(line).trim_end() == "  + new"));
            for line in &lines[..lines.len() - 1] {
                let plain = strip_ansi(line);
                assert!(line.starts_with(&bg.to_bg()));
                assert_eq!(crate::utils::visible_width(line), 40);
                assert!(
                    plain.starts_with("  ") && plain.ends_with("  "),
                    "{plain:?}"
                );
            }
            // Diff foreground resets must restore the full panel background.
            let diff = lines
                .iter()
                .find(|line| strip_ansi(line).contains("+ new"))
                .unwrap();
            assert!(diff.matches(&bg.to_bg()).count() > 1);
        }
    }

    #[test]
    fn generic_panel_spacing_is_configurable_without_global_mutation() {
        let mut theme = Theme::default();
        theme.spacing.tool_panel_padding_x = 4;
        theme.spacing.tool_panel_padding_y = 2;
        let tool = ToolExecutionComponent::new("edit", "{}");
        tool.set_result("done", false);
        tool.set_diff(vec!["+ new".into()]);
        let lines = tool.render_panel(40, &theme);
        let blank = theme.colors.tool_success_bg.bg(&" ".repeat(40));
        assert_eq!(lines[0..2], [blank.clone(), blank.clone()]);
        assert_eq!(
            lines[lines.len() - 3..],
            [blank.clone(), blank, String::new()],
            "panel ends with padding + trailing spacer"
        );
        assert!(strip_ansi(&lines[2]).starts_with("    ✓ edit"));
        assert!(lines
            .iter()
            .any(|line| strip_ansi(line).trim_end() == "    │ done"));
        assert!(lines
            .iter()
            .any(|line| strip_ansi(line).trim_end() == "    + new"));

        theme.spacing.tool_panel_padding_x = 0;
        theme.spacing.tool_panel_padding_y = 0;
        let lines = tool.render_panel(40, &theme);
        assert!(strip_ansi(&lines[0]).starts_with("✓ edit"));
        assert_eq!(strip_ansi(&lines[lines.len() - 2]).trim_end(), "+ new");
    }

    #[test]
    fn generic_output_wraps_between_both_insets() {
        let theme = Theme::default();
        let output = "abcdefghijklmnop你好世界qrstuvwxyz";
        let tool = ToolExecutionComponent::new("read", "{}");
        tool.set_result(output, false);
        for expanded in [false, true] {
            tool.set_expanded(expanded);
            let lines = tool.render_panel(12, &theme);
            let reconstructed: String = lines
                .iter()
                .filter_map(|line| {
                    let plain = strip_ansi(line);
                    plain
                        .strip_prefix("  │ ")
                        .map(|body| body.trim_end().to_string())
                })
                .collect();
            assert_eq!(reconstructed, output);
            for line in &lines[..lines.len() - 1] {
                let plain = strip_ansi(line);
                assert_eq!(crate::utils::visible_width(line), 12);
                assert!(plain.starts_with("  ") && plain.ends_with("  "));
            }
        }
    }

    #[test]
    fn generic_panels_handle_narrow_widths_and_large_padding() {
        let mut theme = Theme::default();
        theme.spacing.tool_panel_padding_x = usize::MAX;
        let tool = ToolExecutionComponent::new("edit", r#"{"path":"很长的文件.rs"}"#);
        tool.set_result("你好世界abcdefghijklmnop", false);
        tool.set_diff(vec!["+ 中文abcdefghijklmnop".into()]);
        for expanded in [false, true] {
            tool.set_expanded(expanded);
            for markdown in [false, true] {
                tool.set_result_markdown(markdown);
                for width in 0..=16 {
                    let lines = tool.render_panel(width, &theme);
                    assert!(
                        lines
                            .iter()
                            .all(|line| crate::utils::visible_width(line) <= width),
                        "width {width}: {lines:?}"
                    );
                    if width == 0 {
                        assert!(lines.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn expanded_args_wrap_and_diff_stays_inside_the_panel() {
        let theme = Theme::default();
        let tool = ToolExecutionComponent::new("edit", r#"{"path":"abcdefghijklmnop.rs"}"#);
        let diff = "+ 你好abcdefghijklmnopqrst";
        tool.set_result("done", false);
        tool.set_diff(vec![theme.colors.tool_diff_added.fg(diff)]);
        tool.set_expanded(true);
        let lines = tool.render_panel(16, &theme);
        let compact: String = strip_ansi(&lines.join("\n"))
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(compact.contains("abcdefghijklmnop.rs"));
        assert!(compact.contains("+你好abc"));
        assert!(compact.contains('…'));
        assert!(compact.contains("args:"));
        for line in &lines[..lines.len() - 1] {
            let plain = strip_ansi(line);
            assert_eq!(crate::utils::visible_width(line), 16);
            assert!(plain.starts_with("  ") && plain.ends_with("  "));
        }
    }

    #[test]
    fn test_tool_execution_basic() {
        let tool = ToolExecutionComponent::new("read", "file.txt");
        let lines = tool.render(80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_tool_execution_status() {
        let tool = ToolExecutionComponent::new("bash", "ls -la");

        assert_eq!(tool.status(), ToolStatus::Pending);

        tool.set_running();
        assert_eq!(tool.status(), ToolStatus::Running);

        tool.set_result("output", false);
        assert_eq!(tool.status(), ToolStatus::Completed);
    }

    #[test]
    fn test_tool_execution_expanded() {
        let tool = ToolExecutionComponent::new("edit", "file.rs");
        tool.set_expanded(true);
        tool.set_result("success", false);

        let lines = tool.render(80);
        assert!(lines.len() > 1); // Should have more lines when expanded
    }

    #[test]
    fn test_tool_execution_error() {
        let tool = ToolExecutionComponent::new("bash", "invalid_command");
        tool.set_result("command not found", true);

        let lines = tool.render(80);
        let joined = lines.join("\n");
        // Failed state: error-colored ✗ glyph + the result text echoed.
        assert!(
            joined.contains("command not found"),
            "result text missing: {joined}"
        );
        assert!(joined.contains('✗'), "error glyph missing: {joined}");
    }

    #[test]
    fn long_tool_header_wraps_instead_of_truncating() {
        let title = "A VERY LONG EXTENSION TOOL NAME";
        let tool = ToolExecutionComponent::new("extension_tool", "{}");
        tool.set_display_title(title);
        tool.set_running();
        let lines = tool.render(16);
        let rendered = crate::ansi::strip_ansi(&lines.join("\n"));
        let compact: String = rendered.chars().filter(|ch| !ch.is_whitespace()).collect();
        let expected: String = title.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            compact.contains(&expected),
            "header was truncated: {rendered}"
        );
        assert!(lines
            .iter()
            .all(|line| crate::utils::visible_width(line) <= 16));
    }

    #[test]
    fn tool_output_wraps_with_a_stable_gutter_and_background() {
        let tool = ToolExecutionComponent::new("read", r#"{"path":"wide.txt"}"#);
        tool.set_result("0123456789ABCDEFGHIJ", false);
        let rendered = tool.render(12);
        let body: Vec<String> = rendered
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .filter(|line| line.contains('│'))
            .collect();
        assert!(body.len() >= 2, "long output should wrap: {body:?}");
        assert!(body.iter().all(|line| line.starts_with("  │ ")));
        assert!(rendered
            .iter()
            .all(|line| crate::utils::visible_width(line) <= 12));
        // Nested foreground resets must re-apply the panel background.
        assert!(rendered
            .iter()
            .any(|line| line.matches("\x1b[48;2;38;48;52m").count() > 1));
    }

    #[test]
    fn markdown_result_renders_tables_instead_of_literal_pipes() {
        let tool = ToolExecutionComponent::new("todo", r#"{"action":"list"}"#);
        tool.set_result_markdown(true);
        tool.set_result(
            "**Todos** | 1 done\n\n| ID | Task |\n|----|------|\n| #1 | ship |",
            false,
        );
        let rendered = crate::ansi::strip_ansi(&tool.render(60).join("\n"));
        // The markdown component lays the table out with box-drawing borders
        // instead of the raw `| a | b |` source.
        assert!(
            rendered.contains('┌') && rendered.contains('│'),
            "table not rendered: {rendered}"
        );
        assert!(
            !rendered.contains("| ID |"),
            "raw markdown table leaked: {rendered}"
        );
        assert!(
            !rendered.contains("**Todos**"),
            "raw bold marker leaked: {rendered}"
        );
        assert!(rendered.contains("Todos"), "text missing: {rendered}");

        // The default (non-markdown) mode must keep rendering the same body as
        // plain text with the `│` gutter.
        let plain = ToolExecutionComponent::new("todo", r#"{"action":"list"}"#);
        plain.set_result("| ID | Task |", false);
        let plain_rendered = crate::ansi::strip_ansi(&plain.render(60).join("\n"));
        assert!(
            plain_rendered.contains("| ID | Task |"),
            "plain tools must not be markdown-rendered: {plain_rendered}"
        );
    }

    #[test]
    fn long_tool_output_collapses_and_ctrl_o_expands() {
        let tool = ToolExecutionComponent::new("read", r#"{"path":"large.rs"}"#);
        let output = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        tool.set_result(&output, false);

        let collapsed = tool.render(80);
        let collapsed_plain = crate::ansi::strip_ansi(&collapsed.join("\n"));
        assert!(collapsed_plain.contains("8 more lines (Ctrl+O to expand)"));
        assert!(!collapsed_plain.contains("line 20"));

        tool.set_expanded(true);
        let expanded = tool.render(80);
        let expanded_plain = crate::ansi::strip_ansi(&expanded.join("\n"));
        assert!(expanded_plain.contains("line 20"));
        assert!(expanded_plain.contains("Ctrl+O to collapse"));
    }

    /// The header summarizes the args JSON into a compact signature instead
    /// of dumping the raw `{"path":"..."}` blob. Each builtin tool maps its
    /// key fields to a one-liner.
    /// A `read` of a `SKILL.md` renders as upstream's `[skill] <name>`
    /// invocation box: custom-message background, one collapsed line with an
    /// expand hint, and (when expanded) the frontmatter-stripped skill body.
    #[test]
    fn skill_invocation_renders_native_style_box() {
        let tool = ToolExecutionComponent::new("read", r#"{"path":"/skills/release/SKILL.md"}"#);
        tool.set_skill_name("release");
        tool.set_result(
            "---\nname: release\ndescription: cut a release\n---\n\n# Steps\n\nDo the thing.",
            false,
        );

        let collapsed = tool.render(80);
        let plain = crate::ansi::strip_ansi(&collapsed.join("\n"));
        assert!(
            plain.contains("[skill] release"),
            "collapsed label: {plain}"
        );
        assert!(plain.contains("(Ctrl+O to expand)"), "expand hint: {plain}");
        assert!(!plain.contains("Steps"), "collapsed hides body: {plain}");
        // Uses the custom-message background (dark: #2d2838), not a tool tint.
        assert!(
            collapsed.iter().any(|l| l.contains("\x1b[48;2;45;40;56m")),
            "custom-message bg missing: {collapsed:?}"
        );

        tool.set_expanded(true);
        let expanded = tool.render(80);
        let plain = crate::ansi::strip_ansi(&expanded.join("\n"));
        assert!(plain.contains("[skill]"), "expanded label: {plain}");
        assert!(plain.contains("release"), "expanded name: {plain}");
        assert!(
            plain.contains("Do the thing."),
            "expanded body missing: {plain}"
        );
        // Frontmatter is stripped (upstream shows the parsed skill body only).
        assert!(
            !plain.contains("name: release"),
            "frontmatter leaked: {plain}"
        );
    }

    #[test]
    fn strip_frontmatter_only_strips_a_leading_block() {
        assert_eq!(strip_frontmatter("plain"), "plain");
        assert_eq!(strip_frontmatter("---\nname: x\n---\nbody"), "body");
        // No opening delimiter → untouched.
        assert_eq!(strip_frontmatter("a\n---\nb"), "a\n---\nb");
    }

    #[test]
    fn test_tool_header_args_summary() {
        // `read` → just the path.
        let t = ToolExecutionComponent::new("read", r#"{"path":"src/foo.rs"}"#);
        let joined = t.render(80).join("\n");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(plain.contains("read"), "tool label lowercase: {plain}");
        assert!(plain.contains("src/foo.rs"), "path in summary: {plain}");
        // The raw JSON must NOT leak into the header.
        assert!(!plain.contains("\"path\""), "raw JSON leaked: {plain}");

        // `grep` → pattern · path (no path field → just pattern).
        let t = ToolExecutionComponent::new("grep", r#"{"pattern":"TODO","path":"src"}"#);
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("TODO · src"), "grep summary: {plain}");

        // `edit` with multiple edits → `(N edits)` suffix.
        let t = ToolExecutionComponent::new(
            "edit",
            r#"{"path":"a.rs","edits":[{"oldText":"x"},{"oldText":"y"}]}"#,
        );
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("a.rs (2 edits)"), "edit count: {plain}");
    }

    /// Unknown tools fall back to the first string-valued arg field rather
    /// than showing nothing — keeps the header useful for plugin tools whose
    /// arg shapes the render crate doesn't know.
    #[test]
    fn test_tool_header_unknown_tool_falls_back() {
        let t = ToolExecutionComponent::new("mytool", r#"{"query":"hai"}"#);
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("hai"), "unknown tool summary: {plain}");
    }

    /// The collapsed diff preview caps at DIFF_PREVIEW_LINES and teases the
    /// remaining count; expanding lifts the cap to DIFF_LINE_CAP.
    #[test]
    fn test_tool_diff_preview_then_expand() {
        // Build a diff with 12 changed lines — over the 6-line preview, under
        // the 40-line expanded cap.
        let mut diff = String::new();
        for i in 1..=6 {
            diff.push_str(&format!("-{} old line {}\n", i, i));
            diff.push_str(&format!("+{} new line {}\n", i, i));
        }
        let t = ToolExecutionComponent::new("edit", r#"{"path":"f"}"#);
        t.set_result("ok", false);
        t.set_diff(crate::diff::render_diff(&diff, 80));

        let collapsed = t.render(80);
        // Preview cap = 6 lines + 1 header + 1 result + 1 hint.
        let diff_body = collapsed
            .iter()
            .filter(|l| {
                let p = crate::ansi::strip_ansi(l);
                p.trim_start().starts_with('-') || p.trim_start().starts_with('+')
            })
            .count();
        assert_eq!(
            diff_body, 6,
            "collapsed preview should cap at 6: {collapsed:?}"
        );
        let hint = collapsed
            .iter()
            .map(|l| crate::ansi::strip_ansi(l))
            .find(|l| l.contains("more diff lines"));
        assert!(hint.is_some(), "preview hint missing");
        assert!(
            hint.as_ref().unwrap().contains("Ctrl+O to expand"),
            "hint text: {hint:?}"
        );

        t.set_expanded(true);
        let expanded = t.render(80);
        let diff_body = expanded
            .iter()
            .filter(|l| {
                let p = crate::ansi::strip_ansi(l);
                p.trim_start().starts_with('-') || p.trim_start().starts_with('+')
            })
            .count();
        assert_eq!(diff_body, 12, "expanded should show all 12: {expanded:?}");
    }
}
