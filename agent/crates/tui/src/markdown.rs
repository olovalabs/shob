//! Markdown component for rendering markdown text.
//!
//! Provides ANSI-aware markdown rendering that wraps to the available width
//! without panicking on multibyte (CJK/emoji/box-drawing) characters or
//! leaking escape sequences. Reuses the shared utilities in [`crate::utils`].

use std::any::Any;
use std::sync::{Arc, Mutex};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use super::component::{Component, RenderedDocument};
use super::mermaid::Mermaid;
use crate::ansi::{bold, italic, underline};
use crate::theme::theme;
use crate::utils::{
    apply_background_to_line, visible_width, widest_char_width, wrap_text_with_ansi,
};

/// Markdown rendering options.
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Indentation for code blocks.
    pub code_block_indent: usize,
    /// Maximum width for wrapping (None = use the render width passed in).
    pub max_width: Option<usize>,
    /// Base SGR sequence applied to every rendered line (e.g. the dim italic
    /// style used for thinking blocks). Empty means no base style. The sequence
    /// is re-applied after every nested SGR reset so inner spans (inline code,
    /// links, headings) do not permanently drop the enclosing style.
    pub base_style: String,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            code_block_indent: 2,
            max_width: None,
            base_style: String::new(),
        }
    }
}

/// Markdown - A component that renders markdown text.
pub struct Markdown {
    content: Mutex<String>,
    options: MarkdownOptions,
    padding_x: usize,
    padding_y: usize,
    /// Render cache keyed by (width, theme revision). Markdown parsing + inline
    /// pulldown-cmark work is the dominant per-frame cost of a long transcript;
    /// a finalized message is re-rendered on every frame the transcript is
    /// repainted, so caching the parsed lines is what keeps that O(1) per
    /// unchanged message. Mirrors upstream `Markdown.cachedText/cachedLines`.
    cache: Mutex<Option<MarkdownCache>>,
}

#[derive(Clone)]
struct MarkdownCache {
    width: usize,
    theme_revision: u64,
    lines: Arc<Vec<String>>,
}

impl Markdown {
    /// Create a new markdown component.
    pub fn new(content: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options: MarkdownOptions::default(),
            padding_x,
            padding_y,
            cache: Mutex::new(None),
        }
    }

    /// Create with options.
    pub fn with_options(
        content: impl Into<String>,
        options: MarkdownOptions,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options,
            padding_x,
            padding_y,
            cache: Mutex::new(None),
        }
    }

    /// Apply a base SGR sequence (for example the dim italic ``\x1b[3m\x1b[38;5;244m``
    /// style) to every rendered line. The base style is re-opened after each
    /// nested SGR reset so inner spans like inline code or links do not drop it
    /// for the remainder of the line.
    pub fn with_base_style(mut self, base_style: impl Into<String>) -> Self {
        self.options.base_style = base_style.into();
        self.clear_cache();
        self
    }

    /// Wrap one rendered line with the configured base style, re-opening the
    /// base after nested full, italic, or foreground resets. Returns the line
    /// unchanged when no base style is configured.
    fn apply_base_style(&self, line: &str) -> String {
        let base = self.options.base_style.as_str();
        if base.is_empty() {
            return line.to_string();
        }
        let reset = "\x1b[0m";
        let mut out = String::with_capacity(line.len() + base.len() * 4 + reset.len());
        out.push_str(base);
        let mut rest = line;
        while let Some(idx) = rest.find("\x1b[") {
            out.push_str(&rest[..idx]);
            rest = &rest[idx..];
            let Some(end) = rest
                .as_bytes()
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, byte)| (0x40..=0x7e).contains(*byte))
                .map(|(index, _)| index + 1)
            else {
                break;
            };
            let sequence = &rest[..end];
            out.push_str(sequence);
            // Inner emphasis closes with SGR 23, and links/colors with SGR 39.
            // Both must restore the thinking base just like inline code's SGR 0.
            if matches!(sequence, "\x1b[0m" | "\x1b[m" | "\x1b[23m" | "\x1b[39m") {
                out.push_str(base);
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        // Terminate the line so the base style cannot bleed into the next row
        // (rendered lines are written back-to-back with no implicit reset).
        out.push_str(reset);
        out
    }

    /// Set the content.
    pub fn set_content(&self, content: impl Into<String>) {
        if let Ok(mut c) = self.content.lock() {
            *c = content.into();
            if let Ok(mut cache) = self.cache.lock() {
                *cache = None;
            }
        }
    }

    /// Drop the cached render output.
    fn clear_cache(&self) {
        // Always acquire content before cache, including invalidation.
        if let Ok(_content) = self.content.lock() {
            if let Ok(mut cache) = self.cache.lock() {
                *cache = None;
            }
        }
    }

    fn shared_lines(&self, width: usize) -> Arc<Vec<String>> {
        let content = self.content.lock().unwrap_or_else(|e| e.into_inner());
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let theme_revision = crate::theme::theme_revision();
        if let Some(cached) = cache.as_ref() {
            if cached.width == width && cached.theme_revision == theme_revision {
                return Arc::clone(&cached.lines);
            }
        }
        let lines = Arc::new(self.render_markdown(&content, width));
        *cache = Some(MarkdownCache {
            width,
            theme_revision,
            lines: Arc::clone(&lines),
        });
        lines
    }

    /// Get the content.
    pub fn get_content(&self) -> String {
        self.content.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// The left-pad prefix applied to every rendered line.
    fn pad(&self) -> String {
        " ".repeat(self.padding_x)
    }

    /// Available content width accounting for horizontal padding.
    fn content_width(&self, width: usize) -> usize {
        width.saturating_sub(self.padding_x * 2)
    }

    /// Render markdown to lines.
    fn render_markdown(&self, content: &str, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let mut lines: Vec<String> = Vec::new();
        let indent = " ".repeat(self.options.code_block_indent);
        let pad = self.pad();
        let cwidth = self.content_width(width);

        // Add top padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        // Pre-scan for tables: collect consecutive table-ish lines.
        let raw_lines: Vec<&str> = content.lines().collect();
        let mut i = 0;
        // Markdown allows a list item's paragraph to continue on an indented
        // line. Keep the visual text offset so continuation lines don't jump
        // back to the left edge of the transcript.
        let mut list_continuation_indent: Option<usize> = None;

        // Code-fence state. Fenced blocks are rendered as a compact terminal
        // panel rather than echoing Markdown's literal ``` markers.
        let mut in_code_block = false;
        let mut code_fence_char = '`';
        let mut code_language = String::new();
        // Syntax highlighter for the current fenced block (highlight.js
        // analogue; see `syntax_highlight`). Carries block-comment state
        // across lines. `None` outside a code block.
        let mut code_highlighter: Option<crate::syntax_highlight::Highlighter> = None;
        let mut mermaid_source: Vec<String> = Vec::new();
        while i < raw_lines.len() {
            let line = raw_lines[i];

            // Streamed partial closing fences (pi #5825): while the assistant
            // streams the final ```` ``` ````, the tail arrives one char at a
            // time (``, ```` ``` ````…). Rendering those would flicker the block
            // (content shrinks, then the real fence closes) — and a lone ``
            // or `````` would be mis-rendered as ordinary text. Trim any line
            // that is a partial fence run (1-2 fence chars, whole line).
            if is_partial_fence(line) {
                i += 1;
                continue;
            }

            // ---- Code fences ----
            let fence = fence_info(line);
            if let Some((fence_char, lang)) = fence {
                list_continuation_indent = None;
                // Inside a block, only the matching fence closes it. This keeps
                // backticks embedded in a ~~~ block from collapsing the panel.
                if !in_code_block {
                    in_code_block = true;
                    code_fence_char = fence_char;
                    code_language = lang.to_ascii_lowercase();
                    code_highlighter =
                        Some(crate::syntax_highlight::Highlighter::new(&code_language));
                    mermaid_source.clear();
                    // Fence language names (for example `text`, `rust`, or
                    // `bash`) are metadata, not code content. Keep ordinary
                    // code blocks content-only; Mermaid renders its own
                    // diagram label when the matching fence closes.
                } else if fence_char == code_fence_char {
                    if code_language == "mermaid" {
                        let component = Mermaid::new(mermaid_source.join("\n"));
                        let mermaid_lines = component.render(cwidth);
                        lines.extend(mermaid_lines.into_iter().map(|line| format!("{pad}{line}")));
                    }
                    in_code_block = false;
                    code_language.clear();
                    code_highlighter = None;
                    mermaid_source.clear();
                    // The closing fence is structural only; do not draw a
                    // bottom border around the code surface.
                } else {
                    // A non-matching fence is code content.
                    push_code_line(
                        line,
                        &pad,
                        &indent,
                        cwidth,
                        &mut lines,
                        code_highlighter.as_mut(),
                    );
                }
                i += 1;
                continue;
            }

            if in_code_block {
                list_continuation_indent = None;
                if code_language == "mermaid" {
                    mermaid_source.push(line.to_string());
                    i += 1;
                    continue;
                }
                // Code is never reflowed in the source sense — tabs are
                // normalized and long physical lines wrap (see `push_code_line`)
                // so a long command stays copyable instead of being ellipsized.
                push_code_line(
                    line,
                    &pad,
                    &indent,
                    cwidth,
                    &mut lines,
                    code_highlighter.as_mut(),
                );
                i += 1;
                continue;
            }

            // ---- Tables ----
            if is_table_separator(raw_lines.get(i + 1)) && is_table_row(line) {
                list_continuation_indent = None;
                // Collect the table block.
                let mut table_lines: Vec<&str> = Vec::new();
                table_lines.push(line);
                i += 1;
                // skip separator
                table_lines.push(raw_lines[i]);
                i += 1;
                while i < raw_lines.len() && is_table_row(raw_lines[i]) {
                    table_lines.push(raw_lines[i]);
                    i += 1;
                }
                render_table(&table_lines, &pad, cwidth, &mut lines, |cell| {
                    self.render_inline(cell)
                });
                continue;
            }

            // ---- Headers ----
            // Headings are rendered with the mdHeading color and bold text;
            // h1/h2 are additionally underlined. Markdown markers are hidden.
            if let Some(rest) = strip_header(line, "######") {
                list_continuation_indent = None;
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#####") {
                list_continuation_indent = None;
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "####") {
                list_continuation_indent = None;
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "###") {
                list_continuation_indent = None;
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "##") {
                list_continuation_indent = None;
                let h = colors
                    .md_heading
                    .fg(&bold(&underline(&self.render_inline(rest))));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#") {
                list_continuation_indent = None;
                let h = colors
                    .md_heading
                    .fg(&bold(&underline(&self.render_inline(rest))));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(item) = parse_list_item(line) {
                let marker = match item.marker {
                    ListMarker::Bullet => colors.md_list_bullet.fg("•"),
                    ListMarker::Ordered(marker) => colors.md_list_bullet.fg(&marker),
                    ListMarker::Task(marker) => colors.md_list_bullet.fg(&marker.to_string()),
                };
                let prefix = format!("{}{} ", "  ".repeat(item.depth + 1), marker);
                list_continuation_indent = Some(visible_width(&prefix));
                push_wrapped_with_prefix(
                    &pad,
                    &prefix,
                    &self.render_inline(item.body),
                    cwidth,
                    &mut lines,
                );
            } else if let Some((depth, body)) = parse_blockquote(line) {
                list_continuation_indent = None;
                let border = colors.md_quote_border.fg("│");
                let prefix = format!("  {}", format!("{border} ").repeat(depth));
                let body = colors.md_quote.fg(&italic(&self.render_inline(body)));
                push_wrapped_with_prefix(&pad, &prefix, &body, cwidth, &mut lines);
            } else if line.trim().starts_with("---") || line.trim().starts_with("***") {
                list_continuation_indent = None;
                // Horizontal rule (capped like the TS reference), mdHr colored.
                let rule_w = width.min(80).saturating_sub(self.padding_x * 2);
                lines.push(format!(
                    "{pad}{}",
                    colors.md_hr.fg(&"─".repeat(rule_w.max(1)))
                ));
            } else if line.trim().is_empty() {
                list_continuation_indent = None;
                // Empty line — emit a (padded) blank so spacing is preserved.
                lines.push(String::new());
            } else if let Some(indent) =
                list_continuation_indent.filter(|_| line.starts_with(' ') || line.starts_with('\t'))
            {
                push_wrapped_with_prefix(
                    &pad,
                    &" ".repeat(indent),
                    &self.render_inline(line.trim()),
                    cwidth,
                    &mut lines,
                );
            } else {
                list_continuation_indent = None;
                // Regular text with inline formatting.
                push_wrapped(&pad, &self.render_inline(line), cwidth, &mut lines);
            }

            i += 1;
        }

        // Add bottom padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        if lines.is_empty() {
            lines.push(String::new());
        }

        // Apply the configured base style (e.g. the dim italic thinking style)
        // per rendered line, re-opening it after every nested reset.
        if !self.options.base_style.is_empty() {
            for line in lines.iter_mut() {
                *line = self.apply_base_style(line);
            }
        }

        lines
    }

    /// Render inline CommonMark/GFM with terminal-specific ANSI styling.
    fn render_inline(&self, text: &str) -> String {
        let colors = theme().colors;
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);

        // pulldown-cmark parses complete documents and has no inline-only
        // entry point. A plain prefix keeps block-looking text in inline
        // context; it is discarded from the first text event below.
        let source = format!("x {text}");
        let mut discard_prefix = true;
        let mut frames = vec![InlineFrame::root()];

        for event in Parser::new_ext(&source, options) {
            match event {
                Event::Start(Tag::Strong) => frames.push(InlineFrame::new(InlineKind::Bold)),
                Event::Start(Tag::Emphasis) => frames.push(InlineFrame::new(InlineKind::Italic)),
                Event::Start(Tag::Strikethrough) => {
                    frames.push(InlineFrame::new(InlineKind::Strike));
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    frames.push(InlineFrame::new(InlineKind::Link(dest_url.into_string())))
                }
                Event::Start(Tag::Image { dest_url, .. }) => {
                    frames.push(InlineFrame::new(InlineKind::Image(dest_url.into_string())))
                }
                Event::End(
                    TagEnd::Strong
                    | TagEnd::Emphasis
                    | TagEnd::Strikethrough
                    | TagEnd::Link
                    | TagEnd::Image,
                ) => close_inline_frame(&mut frames, &colors),
                Event::Text(value) => {
                    let mut value = value.as_ref();
                    if discard_prefix {
                        value = value.strip_prefix("x ").unwrap_or(value);
                        discard_prefix = false;
                    }
                    frames
                        .last_mut()
                        .expect("root inline frame")
                        .text
                        .push_str(value);
                }
                Event::Code(code) => {
                    let chip = format!(" {} ", colors.md_code.fg(code.trim()));
                    frames
                        .last_mut()
                        .expect("root inline frame")
                        .text
                        .push_str(&colors.md_code_bg.bg(&chip));
                }
                Event::InlineMath(value) => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push_str(&format!("${value}$")),
                Event::DisplayMath(value) => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push_str(&format!("$${value}$$")),
                Event::Html(value) | Event::InlineHtml(value) => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push_str(&value),
                Event::FootnoteReference(label) => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push_str(&format!("[^{label}]")),
                Event::SoftBreak => frames.last_mut().expect("root inline frame").text.push(' '),
                Event::HardBreak => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push('\n'),
                Event::TaskListMarker(checked) => frames
                    .last_mut()
                    .expect("root inline frame")
                    .text
                    .push(if checked { '☒' } else { '☐' }),
                Event::Rule | Event::Start(_) | Event::End(_) => {}
            }
        }

        while frames.len() > 1 {
            close_inline_frame(&mut frames, &colors);
        }
        frames.pop().expect("root inline frame").text
    }
}

#[derive(Debug)]
enum InlineKind {
    Root,
    Bold,
    Italic,
    Strike,
    Link(String),
    Image(String),
}

#[derive(Debug)]
struct InlineFrame {
    kind: InlineKind,
    text: String,
}

impl InlineFrame {
    fn root() -> Self {
        Self::new(InlineKind::Root)
    }

    fn new(kind: InlineKind) -> Self {
        Self {
            kind,
            text: String::new(),
        }
    }
}

fn close_inline_frame(frames: &mut Vec<InlineFrame>, colors: &crate::theme::ThemeColors) {
    let Some(frame) = (frames.len() > 1).then(|| frames.pop()).flatten() else {
        return;
    };
    let rendered = match frame.kind {
        InlineKind::Root => frame.text,
        InlineKind::Bold => bold(&frame.text),
        InlineKind::Italic => italic(&frame.text),
        InlineKind::Strike => format!("\x1b[9m{}\x1b[29m", frame.text),
        InlineKind::Link(url) => {
            let label = colors.md_link.fg(&underline(&frame.text));
            if crate::ansi::strip_ansi(&frame.text) == url {
                label
            } else {
                format!("{label}{}", colors.md_link_url.fg(&format!(" ({url})")))
            }
        }
        InlineKind::Image(url) => {
            let alt = if frame.text.is_empty() {
                "image".to_string()
            } else {
                frame.text
            };
            format!("{alt}{}", colors.md_link_url.fg(&format!(" ({url})")))
        }
    };
    frames
        .last_mut()
        .expect("root inline frame")
        .text
        .push_str(&rendered);
}

enum ListMarker {
    Bullet,
    Ordered(String),
    Task(char),
}

struct ListItem<'a> {
    depth: usize,
    marker: ListMarker,
    body: &'a str,
}

/// Parse a Markdown list item, including indented/nested lists. The parser is
/// deliberately strict about the marker's following whitespace so ordinary
/// numeric text without a list marker is not rendered as a list.
fn parse_list_item(line: &str) -> Option<ListItem<'_>> {
    let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
    let content = &line[leading..];
    let depth = line[..leading]
        .chars()
        .fold(0usize, |width, ch| width + if ch == '\t' { 4 } else { 1 })
        / 2;

    let (marker, body) = if let Some(rest) = content
        .strip_prefix("- ")
        .or_else(|| content.strip_prefix("* "))
        .or_else(|| content.strip_prefix("+ "))
    {
        if let Some(task_body) = rest.strip_prefix("[ ]") {
            (ListMarker::Task('☐'), task_body.trim_start())
        } else if let Some(task_body) = rest
            .strip_prefix("[x]")
            .or_else(|| rest.strip_prefix("[X]"))
        {
            (ListMarker::Task('☒'), task_body.trim_start())
        } else {
            (ListMarker::Bullet, rest)
        }
    } else {
        let marker_end = content
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if marker_end == 0 {
            return None;
        }
        let marker_char = content.as_bytes().get(marker_end)?;
        if !matches!(marker_char, b'.' | b')')
            || !content
                .as_bytes()
                .get(marker_end + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            return None;
        }
        (
            ListMarker::Ordered(content[..marker_end + 1].to_string()),
            content[marker_end + 2..].trim_start(),
        )
    };

    Some(ListItem {
        depth,
        marker,
        body,
    })
}

/// Parse one or more quote markers, accepting `>quote`, `> quote`, and nested
/// `> > quote` forms used by CommonMark renderers.
fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut body = line.trim_start_matches([' ', '\t']);
    let mut depth = 0;
    while let Some(rest) = body.strip_prefix('>') {
        depth += 1;
        body = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (depth > 0).then_some((depth, body))
}

/// If `line` is a ```` ``` ```` (or ```` ~~~ ````) fence, return the language
/// label (possibly empty). Returns `None` for non-fence lines.
fn fence_info(line: &str) -> Option<(char, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some(('`', trimmed[3..].trim().to_string()))
    } else if trimmed.starts_with("~~~") {
        Some(('~', trimmed[3..].trim().to_string()))
    } else {
        None
    }
}

/// Render one physical code line on a subtle surface, without an outer frame.
///
/// A long line wraps instead of being clipped with an ellipsis. A code block is
/// where long commands and paths show up, and a clipped row *copies* as broken
/// text: on macOS the transcript is copied through the terminal's native
/// selection, so the ellipsis lands in the clipboard and the rest of the
/// command is gone. Wrapping keeps the whole line on screen and in the
/// clipboard; the code surface is re-applied per row so the panel band stays
/// continuous and every rendered row still fits the width.
fn push_code_line(
    line: &str,
    pad: &str,
    indent: &str,
    cwidth: usize,
    out: &mut Vec<String>,
    highlighter: Option<&mut crate::syntax_highlight::Highlighter>,
) {
    let colors = theme().colors;
    // `cwidth` excludes Markdown padding. The code surface starts after the
    // configured indent and fills the remaining width, including blank rows.
    let panel_width = cwidth.saturating_sub(visible_width(indent)).max(1);
    let body_width = panel_width.saturating_sub(2).max(1);
    let expanded = line.replace('\t', "    ");
    // Highlight the whole logical line first (the highlighter carries per-line
    // state), then wrap the styled text so a wrapped row keeps its colors.
    let styled = match highlighter {
        Some(h) => h.highlight(&expanded, &colors),
        None => colors.md_code_block.fg(&expanded),
    };
    for chunk in wrap_text_with_ansi(&styled, body_width) {
        let body = format!(" {chunk}");
        let body =
            apply_background_to_line(&body, panel_width, |text| colors.md_code_block_bg.bg(text));
        out.push(format!("{pad}{indent}{body}"));
    }
}

/// True when the whole line is a run of 1-2 fence chars (``/````/`~`/`~~`)
/// — a streamed, not-yet-complete closing fence. pi trims these (issue
/// #5825) so code blocks don't flicker/shrink while the final fence char
/// streams in; rendering them as text would also misrender the block tail.
fn is_partial_fence(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 2 {
        return false;
    }
    t.chars().all(|c| c == '`' || c == '~')
}

/// Strip a leading `prefix` of `#`s followed by a space from `line`.
fn strip_header<'a>(line: &'a str, hashes: &str) -> Option<&'a str> {
    if let Some(rest) = line.strip_prefix(hashes) {
        if let Some(stripped) = rest.strip_prefix(' ') {
            return Some(stripped);
        }
    }
    None
}

/// True if the line looks like a markdown table row (`| … | … |`).
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() > 1
}

/// True if the line is a table separator (`|---|---|`, `:--:` allowed).
fn is_table_separator(maybe: Option<&&str>) -> bool {
    match maybe {
        Some(line) => {
            let t = line.trim();
            if !t.contains('|') {
                return false;
            }
            let cell = |s: &str| {
                s.chars()
                    .all(|c| c == '-' || c == ':' || c == ' ' || c == '\t')
                    && s.contains('-')
            };
            let inner = t.trim_matches('|');
            inner.split('|').all(cell)
        }
        None => false,
    }
}

/// Parse a table into rendered lines with box-drawing borders.
///
/// Column widths follow the upstream TS renderer (`packages/tui/src/components/markdown.ts`,
/// `renderTable`): cells that do not fit are **wrapped** to extra rows, never
/// clipped behind an ellipsis — a transcript copied through the terminal's
/// native selection would otherwise lose the wrapped-away tail (same failure
/// mode already fixed for code blocks by wrapping instead of clipping).
fn render_table<F>(rows: &[&str], pad: &str, cwidth: usize, out: &mut Vec<String>, render_inline: F)
where
    F: Fn(&str) -> String,
{
    // Split each row into trimmed cells.
    let parse = |row: &str| -> Vec<String> {
        split_table_cells(row)
            .into_iter()
            .map(|cell| render_inline(cell.trim()))
            .collect()
    };
    if rows.len() < 2 {
        return;
    }
    let header = parse(rows[0]);
    let _separator = parse(rows[1]);
    let body: Vec<Vec<String>> = rows[2..].iter().map(|r| parse(r)).collect();
    let ncols = header.len();
    if ncols == 0 {
        return;
    }

    // Border overhead: "| " + (n-1) * " | " + " |" in upstream terms; here the
    // same count expressed for the box-drawing grid: "│ " left plus " │ "
    // between columns plus " │" right.
    let overhead = 3 * ncols + 1;
    let available = cwidth.saturating_sub(overhead);
    if available < ncols {
        // Too narrow for a stable grid: fall back to the raw table text,
        // wrapped like any other paragraph, so nothing is lost.
        for line in wrap_text_with_ansi(&rows.join("\n"), cwidth.max(1)) {
            out.push(format!("{pad}{line}"));
        }
        return;
    }

    // Natural width per column (widest cell).
    let natural: Vec<usize> = (0..ncols)
        .map(|c| {
            std::iter::once(&header)
                .chain(body.iter())
                .map(|r| r.get(c).map(|s| visible_width(s)).unwrap_or(0))
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Floor per column: the widest single glyph the column has to hold. Cells
    // are wrapped per character (the repo-wide convention in
    // `wrap_text_with_ansi`), so any width at or above this renders correctly;
    // below it a double-width glyph (CJK, emoji) spills into the next cell and
    // breaks the grid.
    let min_col: Vec<usize> = (0..ncols)
        .map(|c| {
            std::iter::once(&header)
                .chain(body.iter())
                .map(|r| r.get(c).map(|s| widest_char_width(s)).unwrap_or(1))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();

    // When even the per-glyph floors do not fit, no stable grid exists: fall
    // back to the raw table text, wrapped like any other paragraph, so nothing
    // is lost and no row exceeds the render width.
    if min_col.iter().sum::<usize>() > available {
        for line in wrap_text_with_ansi(&rows.join("\n"), cwidth.max(1)) {
            out.push(format!("{pad}{line}"));
        }
        return;
    }

    // Final column widths: everything fits naturally -> use natural widths;
    // otherwise grow from the min widths in proportion to how far each column
    // is from its natural width, then hand out rounding leftovers.
    let col_w: Vec<usize> = if natural.iter().sum::<usize>() <= available {
        natural
            .iter()
            .zip(&min_col)
            .map(|(n, m)| (*n).max(*m))
            .collect()
    } else {
        let total_grow_potential: usize = natural
            .iter()
            .zip(&min_col)
            .map(|(n, m)| n.saturating_sub(*m))
            .sum();
        let extra = available.saturating_sub(min_col.iter().sum::<usize>());
        let mut widths: Vec<usize> = natural
            .iter()
            .zip(&min_col)
            .map(|(n, m)| {
                let delta = n.saturating_sub(*m);
                let grow = if total_grow_potential > 0 {
                    delta * extra / total_grow_potential
                } else {
                    0
                };
                m + grow
            })
            .collect();
        let mut remaining = available.saturating_sub(widths.iter().sum::<usize>());
        while remaining > 0 {
            let mut grew = false;
            for i in 0..ncols {
                if remaining == 0 {
                    break;
                }
                if widths[i] < natural[i] {
                    widths[i] += 1;
                    remaining -= 1;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        widths
    };

    // Wrap every cell to its column width, then render row by row with the
    // box-drawing grid; rows grow taller instead of losing text. Each body
    // row's leading border doubles as the separator after the previous row.
    let wrap_cell = |cell: &str, w: usize| -> Vec<String> {
        wrap_text_with_ansi(cell, w.max(1))
            .into_iter()
            .map(|line| {
                let vw = visible_width(&line);
                if vw >= w {
                    line
                } else {
                    format!("{line}{}", " ".repeat(w - vw))
                }
            })
            .collect()
    };
    let render_row = |cells: &[String]| -> Vec<String> {
        let cell_lines: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| wrap_cell(c, col_w.get(i).copied().unwrap_or(1)))
            .collect();
        let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1);
        let mut rendered = Vec::with_capacity(height + 1);
        rendered.push(format!(
            "├{}┤",
            col_w
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("┼")
        ));
        for line_idx in 0..height {
            let parts: Vec<String> = cell_lines
                .iter()
                .enumerate()
                .map(|(i, lines)| {
                    // Continuation rows leave exhausted cells empty; pad them
                    // so every grid row spans the full table width.
                    let w = col_w.get(i).copied().unwrap_or(0);
                    let text = lines.get(line_idx).cloned().unwrap_or_default();
                    let vw = visible_width(&text);
                    if vw >= w {
                        text
                    } else {
                        format!("{text}{}", " ".repeat(w - vw))
                    }
                })
                .collect();
            rendered.push(format!("│{}│", parts.join("│")));
        }
        rendered
    };

    out.push(format!(
        "{pad}┌{}┐",
        col_w
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("┬")
    ));
    for line in render_row(&header).into_iter().skip(1) {
        out.push(format!("{pad}{line}"));
    }
    for row in &body {
        let mut cells = row.clone();
        cells.resize(ncols, String::new());
        for line in render_row(&cells) {
            out.push(format!("{pad}{line}"));
        }
    }
    out.push(format!(
        "{pad}└{}┘",
        col_w
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("┴")
    ));
}

/// Split a table row without treating escaped pipes or pipes inside code spans
/// as column separators.
fn split_table_cells(row: &str) -> Vec<String> {
    let inner = row.trim().trim_matches('|');
    let mut cells = vec![String::new()];
    let mut escaped = false;
    let mut code_ticks = 0usize;

    for ch in inner.chars() {
        if escaped {
            cells.last_mut().expect("table has one cell").push(ch);
            escaped = false;
        } else if ch == '\\' {
            cells.last_mut().expect("table has one cell").push(ch);
            escaped = true;
        } else if ch == '`' {
            code_ticks ^= 1;
            cells.last_mut().expect("table has one cell").push(ch);
        } else if ch == '|' && code_ticks == 0 {
            cells.push(String::new());
        } else {
            cells.last_mut().expect("table has one cell").push(ch);
        }
    }
    cells
}

/// Wrap `rendered` (an already-styled line possibly with leading pad) into the
/// available width and push each sub-line into `out`.
fn push_wrapped(pad: &str, rendered: &str, cwidth: usize, out: &mut Vec<String>) {
    if cwidth == 0 {
        out.push(rendered.to_string());
        return;
    }
    // Padding is layout chrome, not part of `rendered`; applying it here keeps
    // first and continuation lines aligned (the old renderer omitted padding
    // from the first line of ordinary paragraphs and headings).
    let wrapped = wrap_text_with_ansi(rendered, cwidth);
    if wrapped.is_empty() {
        out.push(String::new());
        return;
    }
    for sub in wrapped {
        out.push(format!("{pad}{sub}"));
    }
}

/// Wrap text after a semantic prefix (list bullet, quote border, etc.). The
/// first line gets the prefix; every continuation starts under the text rather
/// than under the marker, which makes long assistant responses much easier to
/// scan in a narrow terminal.
fn push_wrapped_with_prefix(
    pad: &str,
    prefix: &str,
    rendered: &str,
    cwidth: usize,
    out: &mut Vec<String>,
) {
    let prefix_width = visible_width(prefix);
    let body_width = cwidth.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_text_with_ansi(rendered, body_width);
    if wrapped.is_empty() {
        out.push(format!("{pad}{prefix}"));
        return;
    }
    let continuation = " ".repeat(prefix_width);
    for (index, line) in wrapped.iter().enumerate() {
        let line_prefix = if index == 0 { prefix } else { &continuation };
        // `wrap_text_with_ansi` preserves the separating space at a wrap
        // boundary. It is useful for paragraphs, but here it would make a
        // continuation one column farther right than the list's text.
        let body = if index == 0 {
            line.as_str()
        } else {
            line.trim_start()
        };
        out.push(format!("{pad}{line_prefix}{body}"));
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        self.shared_lines(width).as_ref().clone()
    }

    fn render_document(&self, width: usize) -> RenderedDocument {
        RenderedDocument::from_shared(self.shared_lines(width))
    }

    fn invalidate(&self) {
        self.clear_cache();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_rows_reuse_and_regenerate_on_mutation_and_width() {
        let md = Markdown::new("abcdefgh", 0, 0);
        let first = md.shared_lines(80);
        let _document = md.render_document(80);
        assert!(Arc::ptr_eq(&first, &md.shared_lines(80)));
        assert_eq!(first.as_ref(), &md.render(80));
        let narrow = md.shared_lines(4);
        assert!(!Arc::ptr_eq(&first, &narrow));
        assert_eq!(narrow.as_ref(), &vec!["abcd", "efgh"]);
        md.set_content("**new text**");
        let changed = md.shared_lines(80);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(crate::ansi::strip_ansi(&changed.join("\n")), "new text");
        md.invalidate();
        let refreshed = md.shared_lines(80);
        assert!(!Arc::ptr_eq(&changed, &refreshed));
        assert_eq!(changed.as_ref(), refreshed.as_ref());
    }

    #[test]
    fn test_markdown_headers() {
        let md = Markdown::new("# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6", 0, 0);
        let lines = md.render(80);
        let plain: Vec<String> = lines
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();
        assert_eq!(plain, vec!["H1", "H2", "H3", "H4", "H5", "H6"]);
    }

    #[test]
    fn test_commonmark_inline_parser_handles_nested_spans_and_images() {
        let md = Markdown::new(
            "This is ***nested***, ``a ` b``, <https://example.com>, ![plot](plot.png)",
            0,
            0,
        );
        let rendered = md.render(120).join("\n");
        let plain = crate::ansi::strip_ansi(&rendered);

        assert_eq!(
            plain,
            "This is nested,  a ` b , https://example.com, plot (plot.png)"
        );
        assert!(rendered.contains("\x1b[1m"), "nested strong style missing");
        assert!(rendered.contains("\x1b[3m"), "nested emphasis missing");
        assert!(
            rendered.contains("\x1b[48;2;48;52;59m"),
            "multi-backtick code span should use the code chip"
        );
    }

    #[test]
    fn test_markdown_inline() {
        let md = Markdown::new("This is **bold** and *italic* text", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
    }

    #[test]
    fn test_markdown_code() {
        let md = Markdown::new("Use `code` here", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
        assert!(
            lines[0].contains("\x1b[48;2;48;52;59m"),
            "inline code should use the theme chip background"
        );
    }

    #[test]
    fn test_markdown_no_panic_on_wide_cjk() {
        // A CJK + bold line at a narrow width used to panic via the byte-slice
        // truncation in the old renderer (markdown.rs:136).
        let md = Markdown::new("**你好世界**这是一段很长的中文内容需要自动换行", 1, 0);
        let lines = md.render(10);
        // Must not panic and must produce more than one wrapped line.
        assert!(lines.len() > 1);
    }

    #[test]
    fn test_markdown_wraps_long_line() {
        let long = "a".repeat(100);
        let md = Markdown::new(long, 0, 0);
        let lines = md.render(20);
        assert!(lines.len() > 1, "a 100-col line at width 20 should wrap");
    }

    #[test]
    fn test_markdown_code_block_has_background_without_frame() {
        let md = Markdown::new("```rust\nlet x = 1;\n```", 0, 0);
        let joined = md.render(80).join("\n");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(!plain.contains("rust"), "language label leaked: {plain}");
        assert!(plain.contains(" let x = 1;"), "missing code body: {plain}");
        assert!(
            !plain.contains(['╭', '╰', '│']),
            "code block must not draw an outer frame: {plain}"
        );
        assert!(
            joined.contains("\x1b[48;2;40;44;52m"),
            "code panel background missing: {joined:?}"
        );
        assert!(
            !plain.contains("```"),
            "literal fences should be hidden: {plain}"
        );
    }

    #[test]
    fn code_fence_language_metadata_is_not_rendered() {
        let md = Markdown::new(
            "```text\nplain block\n```\n\n```bash\necho hello\n```",
            0,
            0,
        );
        let plain = crate::ansi::strip_ansi(&md.render(80).join("\n"));
        assert!(plain.contains("plain block"));
        assert!(plain.contains("echo hello"));
        assert!(!plain.contains("text"), "text label leaked: {plain}");
        assert!(!plain.contains("bash"), "bash label leaked: {plain}");
    }

    #[test]
    fn test_mermaid_fence_uses_diagram_renderer() {
        let md = Markdown::new("```mermaid\nflowchart LR\nA[Start] --> B[End]\n```", 0, 0);
        let plain = crate::ansi::strip_ansi(&md.render(80).join("\n"));
        assert!(plain.contains("mermaid"));
        assert!(
            plain.contains("Start ──▶ End"),
            "diagram body missing: {plain}"
        );
        assert!(
            !plain.contains("flowchart LR"),
            "raw mermaid header leaked: {plain}"
        );
    }

    #[test]
    fn test_markdown_padding_applies_to_first_and_wrapped_lines() {
        let md = Markdown::new("abcdefgh", 2, 0);
        let lines = md.render(6);
        assert_eq!(lines, vec!["  ab", "  cd", "  ef", "  gh"]);
    }

    #[test]
    fn test_markdown_code_expands_tabs_and_wraps_long_lines() {
        let md = Markdown::new("```\n\tlet value = 123456;\n```", 0, 0);
        let lines = md.render(14);
        let plain: Vec<String> = lines
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();
        assert!(
            plain[0].starts_with("       let"),
            "tab/indent lost: {plain:?}"
        );
        // A long code line wraps rather than clipping with an ellipsis, so the
        // whole line survives rendering and therefore survives a native-selection
        // copy (macOS copies the rendered rows, not the component text).
        assert!(
            !plain.iter().any(|line| line.contains('…')),
            "long code line must not clip: {plain:?}"
        );
        assert!(
            plain.iter().filter(|line| !line.trim().is_empty()).count() >= 2,
            "long code line should wrap: {plain:?}"
        );
        let compact: String = plain.iter().map(|line| line.replace(' ', "")).collect();
        assert!(
            compact.contains("letvalue=123456;"),
            "wrapped rows lost text: {plain:?}"
        );
        assert!(plain.iter().all(|line| visible_width(line) <= 14));
    }

    #[test]
    fn test_markdown_task_list_checkbox() {
        let md = Markdown::new("- [x] done\n- [ ] todo", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains('☒'), "checked marker missing");
        assert!(joined.contains('☐'), "unchecked marker missing");
    }

    #[test]
    fn test_markdown_table_renders_borders() {
        let md = Markdown::new("| a | b |\n|---|---|\n| 1 | 2 |", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains('┌'), "missing top-left");
        assert!(joined.contains('┬'), "missing top-mid");
        assert!(joined.contains('┐'), "missing top-right");
        assert!(joined.contains('└'), "missing bottom-left");
    }

    #[test]
    fn test_markdown_table_has_internal_horizontal_lines() {
        let md = Markdown::new(
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |",
            0,
            0,
        );
        let plain = crate::ansi::strip_ansi(&md.render(80).join("\n"));
        assert_eq!(
            plain.matches('├').count(),
            3,
            "missing internal row lines: {plain}"
        );
        assert_eq!(
            plain.matches('┼').count(),
            3,
            "missing internal separators: {plain}"
        );
    }

    #[test]
    fn test_markdown_double_underscore_is_bold() {
        let md = Markdown::new("__strong__", 0, 0);
        let joined = md.render(80).join("\n");
        // `\x1b[1m` is the bold SGR; `\x1b[4m` is underline.
        assert!(joined.contains("\x1b[1m"), "`__` should render bold");
        assert!(
            !joined.contains("\x1b[4m"),
            "`__` should NOT render underline"
        );
    }

    #[test]
    fn test_markdown_hr_capped() {
        let md = Markdown::new("---", 0, 0);
        let lines = md.render(200);
        // HR is capped at 80 cols, not the full 200.
        assert_eq!(visible_width(&lines[0]), 80);
    }

    #[test]
    fn test_markdown_h4_does_not_fall_through() {
        let md = Markdown::new("#### nested", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains("\x1b[1m"), "h4 should be bold");
    }

    #[test]
    fn test_markdown_nested_lists_keep_their_text_indent_when_wrapped() {
        let md = Markdown::new(
            "- parent item that wraps over several terminal columns\n  - child item\n    child continuation",
            0,
            0,
        );
        let plain: Vec<String> = md
            .render(20)
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();

        assert!(
            plain[0].starts_with("  • "),
            "root marker missing: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("    wraps")),
            "wrapped root text should align after its marker: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("    • child")),
            "nested marker missing: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("      child")),
            "continuation should align after nested marker: {plain:?}"
        );
    }

    #[test]
    fn test_markdown_nested_blockquote_and_unclosed_inline_are_literal() {
        let md = Markdown::new(
            "> > nested quote\ntext with *an unfinished emphasis and `unfinished code",
            0,
            0,
        );
        let plain: Vec<String> = md
            .render(80)
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();
        assert!(plain[0].starts_with("  │ │ nested quote"), "{plain:?}");
        assert_eq!(
            plain[1],
            "text with *an unfinished emphasis and `unfinished code"
        );
    }

    #[test]
    fn test_markdown_table_preserves_pipes_in_inline_code() {
        let md = Markdown::new("| Name | Value |\n|---|---|\n| **bold** | `a|b` |", 0, 0);
        let plain = crate::ansi::strip_ansi(&md.render(80).join("\n"));
        assert_eq!(
            plain.matches('┬').count(),
            1,
            "table columns split incorrectly: {plain}"
        );
        assert!(plain.contains("bold"), "inline formatting lost: {plain}");
        assert!(
            plain.contains("a|b"),
            "code pipe split a table cell: {plain}"
        );
    }

    /// The sequences below each occupy two terminal columns as a whole but do
    /// not sum to two from their scalars (ZWJ: 2+0+2 = 4, skin tone: 2+2 = 4,
    /// VS16: 1+0 = 1). A table cell must keep one displayed sequence on one
    /// body row, and the grid must stay aligned, at ordinary widths too.
    const WIDE_SEQUENCES: [&str; 3] = ["👩‍💻", "👍🏽", "❤️"];

    fn assert_sequence_never_split(lines: &[String], seq: &str, context: &str) {
        let rows: Vec<String> = lines.iter().map(|l| crate::ansi::strip_ansi(l)).collect();
        // Find the body row holding the sequence's first scalar, then require
        // the whole cluster on that same row.
        let first = seq.chars().next().unwrap();
        for (idx, row) in rows.iter().enumerate() {
            if row.contains(first) {
                assert!(
                    row.contains(seq),
                    "{seq:?} split across rows at row {idx} ({context}): {rows:#?}"
                );
            }
        }
    }

    #[test]
    fn test_markdown_table_keeps_zwj_and_modifier_sequences_on_one_row() {
        // Ordinary 80-column terminal with a long neighbouring cell: the wide
        // cell squeezes the sequence column, which is where the split showed.
        for seq in WIDE_SEQUENCES {
            let source = format!(
                "| e | content |\n|---|---|\n| {seq} | {} |",
                "x".repeat(800)
            );
            let lines = Markdown::new(source, 0, 0).render(80);
            assert_sequence_never_split(&lines, seq, "80 columns, long sibling cell");
        }
    }

    #[test]
    fn test_markdown_table_keeps_sequences_at_narrow_widths() {
        // A one-column table is the smallest grid that can still hold a
        // two-cell sequence; the cell floor must be the cluster width, not the
        // widest scalar, or the sequence spills past the border.
        for seq in WIDE_SEQUENCES {
            for width in [6usize, 10, 20] {
                let source = format!("| e |\n|---|\n| {seq} |");
                let lines = Markdown::new(source, 0, 0).render(width);
                assert_sequence_never_split(&lines, seq, &format!("{width} columns"));
                // Every rendered row must be the same display width, or the
                // grid is broken (the sequence counted as 4 instead of 2).
                let widths: Vec<usize> = lines
                    .iter()
                    .map(|l| crate::utils::visible_width(&crate::ansi::strip_ansi(l)))
                    .collect();
                assert!(
                    widths.windows(2).all(|w| w[0] == w[1]),
                    "{seq:?} at {width} columns produced ragged rows: {widths:?} in {lines:#?}"
                );
            }
        }
    }

    #[test]
    fn test_markdown_table_keeps_sequences_among_ascii_and_cjk() {
        // The axis that must not regress while fixing the sequence model:
        // ASCII and CJK tails still survive, and a mixed row keeps its grid.
        let source = "| seq | 内容 |\n|---|---|\n| 👩‍💻 | 中文内容很长的一串文字用于测试换行 |";
        let lines = Markdown::new(source, 0, 0).render(30);
        let plain = crate::ansi::strip_ansi(&lines.join("\n"));
        assert!(plain.contains("👩‍💻"), "sequence lost: {plain}");
        assert!(!plain.contains('…'), "must wrap, not clip: {plain}");
        assert!(
            plain.contains("用于测试换行"),
            "CJK tail lost while wrapping: {plain}"
        );
    }

    #[test]
    fn test_markdown_table_cells_wrap_instead_of_clipping() {
        // Real-world shape: the assistant's PR-split table from session
        // 2026-10-30T16-22-17 (E:\rpi). Long CJK cells in a narrow transcript
        // used to squash the narrow columns to "PR…" / "*(tui)*…" and drop
        // the wide cell's tail behind an ellipsis, so a native-selection copy
        // lost the content — the same failure mode already fixed for code
        // blocks by wrapping instead of clipping.
        let cell = "`ThemeJson` serde 解析层：camelCase→snake_case 映射、`#rrggbb`→`Color`、部分覆盖时回落 default palette;用 pi dark.json/light.json 做 fixture 单测。纯库新增，零行为变化，不碰任何调用点";
        let md = Markdown::new(
            &format!("| PR | 范围 | 内容 |\n|---|---|---|\n| PR 1 | `*(tui)*` | {cell} |"),
            0,
            0,
        );
        let lines = md.render(80);
        let plain = crate::ansi::strip_ansi(&lines.join("\n"));
        // Cells wrap per character, so a multi-character phrase can be split
        // across rows ("*(tui)*" becomes "*(t" / "ui)*"). Per row, strip
        // whitespace and the grid glyphs, then assert on what truncation
        // destroyed: the ellipsis is gone and the cell tail — the part clipping
        // swallowed — survives somewhere in the render.
        let row_text: Vec<String> = lines
            .iter()
            .map(|line| {
                crate::ansi::strip_ansi(line)
                    .chars()
                    .filter(|c| !c.is_whitespace() && !"│┌┐└├┤┬┴┼─".contains(*c))
                    .collect()
            })
            .collect();
        assert!(
            row_text.iter().any(|r| r.contains("PR")),
            "row identifier missing: {plain}"
        );
        assert!(
            row_text.iter().any(|r| r.contains("*(t")),
            "scope cell missing: {plain}"
        );
        assert!(
            !plain.contains('…'),
            "table must wrap cells, not clip with an ellipsis: {plain}"
        );
        assert!(
            row_text.iter().any(|r| r.contains("用点")),
            "cell tail lost to truncation: {plain}"
        );
        // Exhausted columns keep their width on continuation rows: the grid
        // stays rectangular — every row the same visible width, within the
        // render width.
        let row_widths: Vec<usize> = lines.iter().map(|line| visible_width(line)).collect();
        assert!(
            row_widths.iter().all(|w| *w == row_widths[0] && *w <= 80),
            "table grid not rectangular: {row_widths:?}"
        );
        for line in &lines {
            assert!(
                visible_width(line) <= 80,
                "table row exceeds width: {line:?}"
            );
        }
    }

    #[test]
    fn review_pr196_table_cell_keeps_a_sequence_intact_at_terminal_widths() {
        // ZWJ / skin-tone sequences are one display unit: the wrapper must
        // never split them into continuation rows (the woman on one row, the
        // laptop on the next). Reproduces at an ordinary 80-column width and
        // in a one-column cell at width 6 — not only an extremely narrow
        // terminal.
        for emoji in ["👩‍💻", "👍🏽"] {
            let source = format!(
                "| e | content |\n|---|---|\n| {emoji} | {} |",
                "x".repeat(800)
            );
            let lines = Markdown::new(source, 0, 0).render(80);
            let plain = crate::ansi::strip_ansi(&lines.join("\n"));
            assert!(
                plain.contains(emoji),
                "{emoji:?} split across body rows: {plain}"
            );

            let narrow = Markdown::new(format!("| {emoji} |\n|---|"), 0, 0).render(6);
            let plain = crate::ansi::strip_ansi(&narrow.join("\n"));
            assert!(plain.contains(emoji), "{emoji:?} split at width 6: {plain}");
        }
    }

    #[test]
    fn review_pr196_inline_reset_then_combining_mark_survives_render() {
        // The inline renderer's bold reset SGR can land in the same grapheme
        // as a following combining mark; the shared wrapper dropped the
        // visible remainder, so the accent vanished from renderer output.
        let rows = Markdown::new("**a**\u{301}b", 0, 0).render(80);
        let plain = crate::ansi::strip_ansi(&rows.join("\n"));
        assert!(plain.contains('\u{301}'), "accent lost: {plain}");
    }

    #[test]
    fn test_base_style_reopened_after_inline_code_reset() {
        // Regression: inline code (and other colored spans) terminate with an
        // `SGR 0` reset, which used to drop the enclosing base style for the
        // rest of the line. The base must be re-opened after that reset.
        let base = "\x1b[3m\x1b[38;5;244m";
        let md = Markdown::new("think `code` after", 0, 0).with_base_style(base);
        let rendered = md.render(80).join("\n");
        let after = rendered
            .split("code")
            .nth(1)
            .expect("inline code not rendered");
        assert!(
            after.contains("\x1b[38;5;244m"),
            "base style not reopened after inline code: {rendered:?}"
        );
        assert!(
            rendered.ends_with("\x1b[0m"),
            "rendered line must be reset-terminated: {rendered:?}"
        );
    }

    #[test]
    fn test_base_style_applies_to_every_line() {
        // Multi-paragraph thinking must carry the base style on every line,
        // not just the first text run.
        let base = "\x1b[3m\x1b[38;5;244m";
        let md = Markdown::new("first thought\n\nsecond thought", 0, 0).with_base_style(base);
        let lines = md.render(80);
        for line in lines.iter().filter(|l| !l.is_empty()) {
            assert!(
                line.contains("\x1b[38;5;244m"),
                "base style missing on line: {line:?}"
            );
        }
    }

    #[test]
    fn test_markdown_table_grid_survives_narrow_widths() {
        // The wrap-based grid must never overflow the render width and must stay
        // rectangular at every width. The interesting corner is a width so
        // narrow that a double-width glyph (CJK, emoji) no longer fits in one
        // cell: before the per-column floor was derived from the widest glyph,
        // a one-cell column emitted a two-cell line and the grid drifted by one
        // cell per such column (6-wide vs 7-wide rows).
        let cases: Vec<(&str, &str)> = vec![
            (
                "long token",
                "| a | b |\n|---|---|\n| aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | b |",
            ),
            (
                "cjk",
                "| 范围 | 内容 |\n|---|---|\n| *(tui)* | 主题解析层：camelCase→snake_case 映射与回落 |",
            ),
            ("empty cells", "| a | b | c |\n|---|---|---|\n|  | x |  |"),
            (
                "single col",
                "| only |\n|---|\n| a very long single column cell that must wrap somewhere |",
            ),
            (
                "many cols",
                "| a | b | c | d | e | f | g | h |\n|---|---|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |",
            ),
            (
                "wide glyphs",
                "| 表 | 值 |\n|---|---|\n| 宽字符测试 | ＡＢＣＤＥＦＧＨ |",
            ),
            ("emoji", "| e | v |\n|---|---|\n| 🎉🎉🎉 | ok |"),
        ];
        for (name, src) in cases {
            for width in [6usize, 8, 10, 12, 15, 20, 25, 30, 40, 60, 80, 120] {
                let lines = Markdown::new(src, 0, 0).render(width);
                for line in &lines {
                    assert!(
                        visible_width(line) <= width,
                        "{name} @ {width}: row exceeds render width\n{}",
                        lines.join("\n")
                    );
                }
                let grid: Vec<usize> = lines
                    .iter()
                    .filter(|l| {
                        l.contains('│') || l.contains('┌') || l.contains('└') || l.contains('├')
                    })
                    .map(|l| visible_width(l))
                    .collect();
                if grid.len() > 1 {
                    assert!(
                        grid.iter().all(|w| *w == grid[0]),
                        "{name} @ {width}: grid not rectangular {grid:?}\n{}",
                        lines.join("\n")
                    );
                }
            }
        }
    }
}
