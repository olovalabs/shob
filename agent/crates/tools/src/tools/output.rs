//! Bounded, read-only inspection of saved shell output. Never executes a command.

use std::sync::Arc;
use std::time::Instant;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use regex_automata::dfa::{dense, Automaton};
use regex_automata::nfa::thompson;
use regex_automata::util::{primitives::StateID, syntax};
use regex_automata::Input;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::{FileError, FileErrorCode};
use crate::tools::tool_context::ExecutionToolContext;

const CHUNK_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
// Leave space for a continuation notice and a clipped-line marker.
const TEXT_BUDGET: usize = MAX_OUTPUT_BYTES - 512;
const REGEX_MEMORY: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct OutputInput {
    /// Existing full_output_path returned by a shell tool; never reruns it.
    path: String,
    /// First source line (1-based, default 1).
    offset: Option<u64>,
    /// Maximum returned lines (default 100, maximum 2000).
    limit: Option<u32>,
    /// Optional regular expression, evaluated against complete source lines.
    /// Must fit a bounded streaming DFA (4MiB). Unsupported patterns, including
    /// Unicode word-boundary assertions, return an explicit validation error.
    pattern: Option<String>,
    #[serde(default, alias = "ignoreCase")]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    /// Absolute, zero-based file byte position from a previous continuation.
    /// Keep offset/pattern unchanged when following a byte continuation.
    #[serde(alias = "byteOffset")]
    byte_offset: Option<u64>,
}

#[derive(Serialize)]
struct OutputDetails {
    path: String,
    returned_lines: u32,
    /// Original file bytes rendered, excluding line terminators and annotations.
    returned_bytes: u64,
    rendered_bytes: usize,
    next_offset: Option<u64>,
    next_byte_offset: Option<u64>,
    eof: bool,
    search: bool,
    /// Actual bytes fetched, including lookahead and skipped/nonmatching lines.
    scanned_bytes: u64,
    elapsed_ms: u64,
}

struct OutputTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
}

/// Inspect or search a saved shell output file without executing any command.
pub fn create_output_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    Arc::new(OutputTool {
        schema: Tool {
            name: "output".into(),
            description: "Read or search an existing saved shell full_output_path without rerunning commands. Returns numbered source lines, at most 2000 lines/50KiB. Use next_offset and, when present, next_byte_offset as offset and byte_offset to continue without skipping clipped text. Search matches complete lines, including oversized lines; byte continuation preserves the original line's match. Regex must fit a bounded 4MiB streaming DFA; unsupported patterns, including Unicode word-boundary assertions, return a validation error. Read-only.".into(),
            parameters: ai::types::Schema::new(
                serde_json::to_value(schemars::schema_for!(OutputInput)).unwrap_or_default(),
            ),
            constrained_sampling: None,
        },
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for OutputTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "output"
    }

    fn execution_mode(&self) -> agent::ToolExecutionMode {
        agent::ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let started = Instant::now();
        let input: OutputInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("output input invalid: {e}")))?;
        let offset = input.offset.unwrap_or(1);
        let limit = input.limit.unwrap_or(100);
        if offset == 0 || limit == 0 || limit > 2000 {
            return Err(AgentError::Validation(
                "output offset must be >= 1 and limit must be between 1 and 2000".into(),
            ));
        }
        check_cancel(&signal)?;
        let mut matcher = input
            .pattern
            .as_deref()
            .map(|pattern| LineMatcher::new(pattern, input.ignore_case, input.literal))
            .transpose()?;
        let path = self
            .env
            .absolute_path(&input.path, Some(&signal))
            .await
            .map_err(file_error)?;
        let path = path.to_string_lossy().into_owned();
        let file = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(file_error(FileError::aborted())),
            result = self.env.open_read_file(&path) => result.map_err(file_error)?,
        }
        .ok_or_else(|| unsupported(&path, "stable ranged reading is unavailable"))?;
        let length = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(file_error(FileError::aborted())),
            result = file.byte_len() => result.map_err(file_error)?,
        }
        .ok_or_else(|| unsupported(&path, "opened file byte length is unavailable"))?;

        let mut page = Page::new(&input, offset, limit);
        let mut position = 0;
        let mut scanned_bytes = 0;
        let mut stopped = false;
        // Recount from the opened generation, rather than trusting a caller's
        // byte position to describe a line boundary. This also lets searches
        // match the entire original line on a mid-line continuation.
        while position < length && !stopped {
            check_cancel(&signal)?;
            let amount = (length - position).min(CHUNK_BYTES as u64) as usize;
            let bytes = tokio::select! {
                biased;
                _ = signal.cancelled() => return Err(file_error(FileError::aborted())),
                result = file.read_range(position, amount) => result.map_err(file_error)?,
            };
            if bytes.len() != amount {
                return Err(file_error(
                    FileError::new(
                        FileErrorCode::Invalid,
                        "saved output changed during ranged reading",
                    )
                    .with_path(&path),
                ));
            }
            scanned_bytes += bytes.len() as u64;
            for (index, &byte) in bytes.iter().enumerate() {
                // Yield even with immediately-ready in-memory reads, and bound
                // cancellation latency within a large source line.
                if index % 4096 == 0 {
                    tokio::task::yield_now().await;
                    check_cancel(&signal)?;
                }
                if byte == b'\n' {
                    stopped = page.finish_line(position, true, length, &mut matcher);
                    if stopped {
                        break;
                    }
                } else {
                    page.push(byte, position, &mut matcher);
                    // Once a match is certain (or in read mode), a clipped
                    // result need not wait for the rest of an oversized line.
                    if page.captured.len() == MAX_OUTPUT_BYTES + 4
                        && matcher.as_ref().is_none_or(LineMatcher::confirmed)
                    {
                        stopped = page.finish_line(position + 1, false, length, &mut matcher);
                        if stopped {
                            break;
                        }
                    }
                }
                position += 1;
            }
        }
        if !stopped && page.line_start < length {
            page.finish_line(length, false, length, &mut matcher);
        }
        check_cancel(&signal)?;
        page.finish_notice();
        let details = OutputDetails {
            path,
            returned_lines: page.returned_lines,
            returned_bytes: page.returned_bytes,
            rendered_bytes: page.text.len(),
            next_offset: page.next_offset,
            next_byte_offset: page.next_byte_offset,
            eof: page.next_offset.is_none(),
            search: input.pattern.is_some(),
            scanned_bytes,
            elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        };
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(page.text)],
            details: serde_json::to_value(details).unwrap_or_default(),
            ..Default::default()
        })
    }
}

struct Page {
    text: String,
    captured: Vec<u8>,
    line_number: u64,
    line_start: u64,
    capture_start: u64,
    last_byte: Option<u8>,
    offset: u64,
    byte_offset: u64,
    limit: u32,
    returned_lines: u32,
    returned_bytes: u64,
    next_offset: Option<u64>,
    next_byte_offset: Option<u64>,
    search: bool,
}

impl Page {
    fn new(input: &OutputInput, offset: u64, limit: u32) -> Self {
        Self {
            text: String::with_capacity(MAX_OUTPUT_BYTES),
            captured: Vec::with_capacity(MAX_OUTPUT_BYTES + 4),
            line_number: 1,
            line_start: 0,
            capture_start: input.byte_offset.unwrap_or(0),
            last_byte: None,
            offset,
            byte_offset: input.byte_offset.unwrap_or(0),
            limit,
            returned_lines: 0,
            returned_bytes: 0,
            next_offset: None,
            next_byte_offset: None,
            search: input.pattern.is_some(),
        }
    }

    fn push(&mut self, byte: u8, position: u64, matcher: &mut Option<LineMatcher>) {
        if self.line_number >= self.offset {
            // Hold one byte back so CRLF's CR is not part of regex haystacks.
            if let (Some(previous), Some(matcher)) = (self.last_byte, matcher.as_mut()) {
                matcher.push(previous);
            }
            if position >= self.capture_start && self.captured.len() < MAX_OUTPUT_BYTES + 4 {
                self.captured.push(byte);
            }
        }
        self.last_byte = Some(byte);
    }

    /// Return true only when a page boundary leaves unread source text.
    fn finish_line(
        &mut self,
        end: u64,
        newline: bool,
        file_length: u64,
        matcher: &mut Option<LineMatcher>,
    ) -> bool {
        let content_end = end - u64::from(newline && self.last_byte == Some(b'\r'));
        let eligible = self.line_number >= self.offset
            && (self.byte_offset <= self.line_start || self.byte_offset < content_end);
        let matched = if let Some(matcher) = matcher.as_mut() {
            if self.line_number >= self.offset {
                if let Some(last) = self.last_byte {
                    if !(newline && last == b'\r') {
                        matcher.push(last);
                    }
                }
                matcher.finish()
            } else {
                false
            }
        } else {
            true
        };
        if eligible && matched {
            let continued = self.capture_start > self.line_start;
            let prefix = if continued {
                format!(
                    "{} (continued @ byte {}): ",
                    self.line_number, self.capture_start
                )
            } else {
                format!("{}: ", self.line_number)
            };
            if self.text.len() + prefix.len() + 32 > TEXT_BUDGET {
                self.continue_at(self.capture_start);
                return true;
            }
            let available = TEXT_BUDGET.saturating_sub(self.text.len() + prefix.len() + 32);
            let raw_length = content_end.saturating_sub(self.capture_start);
            let stored = self
                .captured
                .len()
                .min(raw_length.min(usize::MAX as u64) as usize);
            let before = self.text.len();
            self.text.push_str(&prefix);
            let consumed = render_prefix(
                &self.captured[..stored],
                available,
                stored as u64 == raw_length,
                &mut self.text,
            );
            if consumed == 0 && raw_length > 0 {
                self.text.truncate(before);
                self.continue_at(self.capture_start);
                return true;
            }
            self.returned_lines += 1;
            self.returned_bytes += consumed as u64;
            if (consumed as u64) < raw_length {
                self.text.push_str(" [clipped; continues below]\n");
                self.continue_at(self.capture_start + consumed as u64);
                return true;
            }
            self.text.push('\n');
            let next = end + u64::from(newline);
            if self.returned_lines >= self.limit && next < file_length {
                self.next_offset = Some(self.line_number + 1);
                return true;
            }
        }
        self.line_number += 1;
        self.line_start = end + u64::from(newline);
        self.capture_start = self.line_start.max(self.byte_offset);
        self.captured.clear();
        self.last_byte = None;
        if let Some(matcher) = matcher {
            matcher.reset();
        }
        false
    }

    fn continue_at(&mut self, position: u64) {
        self.next_offset = Some(self.line_number);
        if position > self.line_start {
            self.next_byte_offset = Some(position);
        }
    }

    fn finish_notice(&mut self) {
        if let Some(offset) = self.next_offset {
            match self.next_byte_offset {
                Some(byte) => self.text.push_str(&format!(
                    "\n[Continue with offset={offset}, byte_offset={byte}; keep the same pattern.]"
                )),
                None => self
                    .text
                    .push_str(&format!("\n[Continue with offset={offset}.]")),
            }
        } else if self.text.is_empty() {
            self.text.push_str(if self.search {
                "No matching lines."
            } else {
                "No lines in this range."
            });
        }
    }
}

/// Lossy text decoding with an exact raw-byte continuation, never splitting a
/// valid UTF-8 character at either a file-chunk or rendered-output boundary.
fn render_prefix(bytes: &[u8], budget: usize, complete: bool, rendered: &mut String) -> usize {
    let start = rendered.len();
    let mut consumed = 0;
    while consumed < bytes.len() {
        let (valid, invalid) = match std::str::from_utf8(&bytes[consumed..]) {
            Ok(text) => (text, None),
            Err(error) => (
                // This prefix has just been validated by from_utf8.
                std::str::from_utf8(&bytes[consumed..consumed + error.valid_up_to()]).unwrap(),
                Some(error.error_len()),
            ),
        };
        let mut take = valid.len().min(budget - (rendered.len() - start));
        while !valid.is_char_boundary(take) {
            take -= 1;
        }
        rendered.push_str(&valid[..take]);
        consumed += take;
        if take < valid.len() {
            break;
        }
        match invalid {
            None => break,
            Some(error_length) => {
                if budget - (rendered.len() - start) < '\u{fffd}'.len_utf8() {
                    break;
                }
                let amount = match error_length {
                    Some(amount) => amount,
                    None if complete => bytes.len() - consumed,
                    None => break,
                };
                rendered.push('\u{fffd}');
                consumed += amount;
            }
        }
    }
    consumed
}

// One bounded streaming DFA per call. Source-line length never affects matcher
// memory, and unsupported regex features fail explicitly during construction.
struct LineMatcher {
    dfa: dense::DFA<Vec<u32>>,
    initial: StateID,
    state: StateID,
    matched: bool,
}

impl LineMatcher {
    fn new(pattern: &str, ignore_case: bool, literal: bool) -> Result<Self, AgentError> {
        let escaped;
        let pattern = if literal {
            escaped = regex::escape(pattern);
            &escaped
        } else {
            pattern
        };
        let dfa = dense::Builder::new()
            .syntax(syntax::Config::new().case_insensitive(ignore_case))
            .thompson(thompson::Config::new()
                .which_captures(thompson::WhichCaptures::None)
                .nfa_size_limit(Some(REGEX_MEMORY)))
            .configure(dense::Config::new()
                .dfa_size_limit(Some(REGEX_MEMORY))
                .determinize_size_limit(Some(REGEX_MEMORY)))
            .build(pattern)
            .map_err(|error| AgentError::Validation(format!(
                "output pattern invalid or unsupported by bounded streaming DFA (4MiB limit): {error}"
            )))?;
        let initial = dfa
            .start_state_forward(&Input::new(b""))
            .map_err(|error| AgentError::Validation(format!("output pattern invalid: {error}")))?;
        Ok(Self {
            dfa,
            initial,
            state: initial,
            matched: false,
        })
    }

    fn reset(&mut self) {
        self.state = self.initial;
        self.matched = false;
    }

    fn push(&mut self, byte: u8) {
        if !self.matched {
            self.state = self.dfa.next_state(self.state, byte);
            self.matched = self.dfa.is_match_state(self.state);
        }
    }

    fn confirmed(&self) -> bool {
        self.matched
    }

    fn finish(&mut self) -> bool {
        self.matched || self.dfa.is_match_state(self.dfa.next_eoi_state(self.state))
    }
}

fn unsupported(path: &str, message: &str) -> AgentError {
    file_error(FileError::new(FileErrorCode::NotSupported, message).with_path(path))
}

fn check_cancel(signal: &CancellationToken) -> Result<(), AgentError> {
    if signal.is_cancelled() {
        Err(file_error(FileError::aborted()))
    } else {
        Ok(())
    }
}

fn file_error(error: FileError) -> AgentError {
    AgentError::Tool(error.to_string())
}
