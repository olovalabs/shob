//! Mirrors `packages/agent/src/harness/utils/shell-output.ts` — the bash tool's
//! output-capture layer wrapping `Shell::exec`.
//!
//! Combines sanitized stdout+stderr into a bounded rolling tail. Once output
//! exceeds display limits, a complete sanitized log is persisted asynchronously.
//! A bounded channel backpressures pipe readers while filesystem writes yield;
//! child status, timeout and cancellation are driven independently of the spool.
//! Optional progress coalescing uses one execution-local timer, including during
//! quiet periods. Snapshots are constructed only when a consumer is notified.
//! The persisted log contains all sanitized output delivered by the backend,
//! not arbitrary output from descendants after shell exit: backend pipe draining
//! has a 100ms grace followed by at most 1MiB of ready bytes per stream.
//! Slow storage can delay the final result while these bounded writes finish,
//! but cannot delay polling shell cancellation, deadlines or tree termination.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::env::{ExecutionEnv, FileContent, ShellExecOptions, ShellOutputChunk};
use crate::error::{ExecutionError, ExecutionErrorCode};
use crate::truncate::{
    trim_to_last_utf8_bytes, truncate_tail, TruncationLimit, TruncationOptions, TruncationResult,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// The 100KB cap on the in-memory `tail_output` rolling buffer.
const MAX_OUTPUT_BYTES: usize = DEFAULT_MAX_BYTES * 2;
const SPOOL_BATCH_BYTES: usize = 64 * 1024;

/// Mirrors `ShellCaptureProgress`.
#[derive(Clone)]
pub struct ShellCaptureProgress {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
    pub last_line_bytes: usize,
}

/// Mirrors `ShellCaptureResult`.
#[derive(Clone)]
pub struct ShellCaptureResult {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
    pub last_line_bytes: usize,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub execution_error: Option<ExecutionError>,
}

/// Mirrors `ShellCaptureOptions`.
pub struct ShellCaptureOptions<'a> {
    pub shell: Option<crate::env::ShellOverride>,
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub inherit_env: bool,
    pub timeout: Option<f64>,
    pub cancel: Option<&'a CancellationToken>,
    pub on_chunk:
        Option<Box<dyn FnMut(&str, &dyn Fn() -> Option<ShellCaptureProgress>) + Send + 'a>>,
    /// Coalesce progress with one timer; `None` preserves per-chunk callbacks.
    pub update_interval: Option<Duration>,
    pub return_execution_errors: bool,
}

impl<'a> Default for ShellCaptureOptions<'a> {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: None,
            inherit_env: true,
            timeout: None,
            cancel: None,
            on_chunk: None,
            update_interval: None,
            return_execution_errors: false,
        }
    }
}

/// Owned by the capture consumer; pipe readers only send bounded chunks.
#[derive(Default)]
struct CaptureState {
    tail_output: String,
    tail_start: usize,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_output_path: Option<PathBuf>,
}

impl CaptureState {
    fn ingest_chunk(&mut self, text: &str) {
        let text_bytes = text.len();
        self.total_bytes += text_bytes;
        let newline_count = text.matches('\n').count();
        self.completed_lines += newline_count;
        let last_newline = text.rfind('\n');
        match last_newline {
            Some(idx) => {
                let trailing = &text[idx + 1..];
                self.current_line_bytes = trailing.len();
                self.has_open_line = !trailing.is_empty();
            }
            None if !text.is_empty() => {
                self.current_line_bytes += text_bytes;
                self.has_open_line = true;
            }
            None => {}
        }
        // Advance a logical start without moving the tail on every pipe read.
        self.tail_output.push_str(text);
        let live = &self.tail_output[self.tail_start..];
        self.tail_start += live.len() - trim_to_last_utf8_bytes(live, MAX_OUTPUT_BYTES).len();
        if self.tail_start >= MAX_OUTPUT_BYTES {
            self.tail_output.drain(..self.tail_start);
            self.tail_start = 0;
        }
    }

    fn create_progress(&self) -> ShellCaptureProgress {
        let tail = &self.tail_output[self.tail_start..];
        let tail_truncation = truncate_tail(tail, TruncationOptions::default());
        let total_lines = self.completed_lines + if self.has_open_line { 1 } else { 0 };
        let truncated = total_lines > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES;
        let truncated_by = if truncated {
            tail_truncation
                .truncated_by
                .or(if self.total_bytes > DEFAULT_MAX_BYTES {
                    Some(TruncationLimit::Bytes)
                } else {
                    Some(TruncationLimit::Lines)
                })
        } else {
            None
        };
        let truncation = TruncationResult {
            truncated,
            truncated_by,
            total_lines,
            total_bytes: self.total_bytes,
            ..tail_truncation
        };
        let output = if truncated {
            truncation.content.clone()
        } else {
            tail.to_string()
        };
        ShellCaptureProgress {
            output,
            truncation,
            full_output_path: self.full_output_path.clone(),
            last_line_bytes: self.current_line_bytes,
        }
    }
}

/// ANSI state belongs to each stream: escape sequences may span pipe reads.
#[derive(Default)]
struct Sanitizer {
    escape: Escape,
}

#[derive(Default, Clone, Copy)]
enum Escape {
    #[default]
    Text,
    Start,
    Csi,
    Osc,
    OscEscape,
    Charset,
}

impl Sanitizer {
    fn push_owned(&mut self, chunk: String) -> String {
        if matches!(self.escape, Escape::Text)
            && !chunk
                .chars()
                .any(|c| matches!(c, '\x00'..='\x08' | '\x0b'..='\x1f' | '\u{fff9}'..='\u{fffb}'))
        {
            return chunk;
        }
        self.push(&chunk)
    }

    fn push(&mut self, chunk: &str) -> String {
        let mut out = String::with_capacity(chunk.len());
        for c in chunk.chars() {
            match self.escape {
                Escape::Text => match c {
                    '\x1b' => self.escape = Escape::Start,
                    '\t' | '\n' => out.push(c),
                    '\x00'..='\x1f' | '\u{fff9}'..='\u{fffb}' => {}
                    _ => out.push(c),
                },
                Escape::Start => {
                    self.escape = match c {
                        '[' => Escape::Csi,
                        ']' => Escape::Osc,
                        '(' | ')' => Escape::Charset,
                        _ => Escape::Text,
                    }
                }
                Escape::Csi => {
                    if ('\x40'..='\x7e').contains(&c) {
                        self.escape = Escape::Text;
                    }
                }
                Escape::Osc => match c {
                    '\x07' => self.escape = Escape::Text,
                    '\x1b' => self.escape = Escape::OscEscape,
                    _ => {}
                },
                Escape::OscEscape => {
                    self.escape = if c == '\\' { Escape::Text } else { Escape::Osc }
                }
                Escape::Charset => self.escape = Escape::Text,
            }
        }
        out
    }
}

#[cfg(test)]
fn sanitize_and_strip_cr(chunk: &str) -> String {
    Sanitizer::default().push(chunk)
}

/// Capture with bounded asynchronous persistence and optional timed progress.
pub async fn execute_shell_with_capture(
    env: &Arc<dyn ExecutionEnv>,
    command: &str,
    options: ShellCaptureOptions<'_>,
) -> Result<ShellCaptureResult, ExecutionError> {
    let (output, mut receiver) = tokio::sync::mpsc::channel(8);
    let exec_opts = ShellExecOptions {
        shell: options.shell,
        cwd: options.cwd,
        env: options.env,
        inherit_env: options.inherit_env,
        timeout: options.timeout,
        cancel: options.cancel,
        on_stdout: None,
        on_stderr: None,
        output: Some(output),
        capture_output: false,
    };
    let execution = env.exec(command, exec_opts);
    let consumer = async {
        let mut state = CaptureState::default();
        let mut sanitizers = [Sanitizer::default(), Sanitizer::default()];
        // Retain the prefix only until display truncation requires a spool.
        let mut prefix = String::new();
        let mut spool_failed = false;
        let mut spool_writer: Option<Box<dyn crate::env::AppendFile>> = None;
        let mut spool_buffer = String::new();
        let mut on_chunk = options.on_chunk;
        let mut dirty = false;
        let mut first = true;
        let mut deadline = tokio::time::Instant::now();
        let timer = tokio::time::sleep_until(deadline);
        tokio::pin!(timer);
        loop {
            let chunk = tokio::select! {
                biased;
                _ = &mut timer, if dirty && options.update_interval.is_some() => {
                    if let Some(cb) = on_chunk.as_mut() {
                        cb("", &|| Some(state.create_progress()));
                    }
                    dirty = false;
                    deadline = tokio::time::Instant::now() + options.update_interval.unwrap();
                    timer.as_mut().reset(deadline);
                    continue;
                }
                chunk = receiver.recv() => match chunk {
                    Some(chunk) => chunk,
                    None => break,
                },
            };
            let text = match chunk {
                ShellOutputChunk::Stdout(text) => sanitizers[0].push_owned(text),
                ShellOutputChunk::Stderr(text) => sanitizers[1].push_owned(text),
            };
            if text.is_empty() {
                continue;
            }
            state.ingest_chunk(&text);
            dirty = true;
            let now = tokio::time::Instant::now();
            if first || options.update_interval.is_none() || now >= deadline {
                if let Some(cb) = on_chunk.as_mut() {
                    cb(&text, &|| Some(state.create_progress()));
                }
                first = false;
                dirty = false;
                deadline = now + options.update_interval.unwrap_or_default();
                timer.as_mut().reset(deadline);
            }

            if !spool_failed {
                if state.full_output_path.is_none() {
                    prefix.push_str(&text);
                }
                let total_lines = state.completed_lines + usize::from(state.has_open_line);
                let needs_spool =
                    state.total_bytes > DEFAULT_MAX_BYTES || total_lines > DEFAULT_MAX_LINES;
                if state.full_output_path.is_none() && !needs_spool {
                    continue;
                }
                if state.full_output_path.is_some() {
                    spool_buffer.push_str(&text);
                    // Coalesce an existing backlog, never wait to create one.
                    // Sparse output still reaches disk immediately.
                    if spool_buffer.len() < SPOOL_BATCH_BYTES && !receiver.is_empty() {
                        continue;
                    }
                }
                let result = {
                    let writing = async {
                        if let Some(path) = &state.full_output_path {
                            append_full_output(env, path, &mut spool_writer, &mut spool_buffer)
                                .await
                                .map(|()| None)
                        } else if needs_spool {
                            init_full_output_file(env, &prefix).await.map(Some)
                        } else {
                            Ok(None)
                        }
                    };
                    tokio::pin!(writing);
                    loop {
                        tokio::select! {
                            result = &mut writing => break result,
                            _ = &mut timer, if dirty && options.update_interval.is_some() => {
                                if let Some(cb) = on_chunk.as_mut() {
                                    cb("", &|| Some(state.create_progress()));
                                }
                                dirty = false;
                                deadline = tokio::time::Instant::now() + options.update_interval.unwrap();
                                timer.as_mut().reset(deadline);
                            }
                        }
                    }
                };
                match result {
                    Ok(Some((path, writer))) => {
                        state.full_output_path = Some(path);
                        spool_writer = writer;
                        prefix = String::new();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!("shell full-output persistence failed: {error}");
                        // Never advertise a partial file as complete.
                        state.full_output_path = None;
                        spool_writer = None;
                        prefix = String::new();
                        spool_failed = true;
                        spool_buffer.clear();
                    }
                }
            }
        }
        if !spool_buffer.is_empty() {
            if let Some(path) = &state.full_output_path {
                if let Err(error) =
                    append_full_output(env, path, &mut spool_writer, &mut spool_buffer).await
                {
                    tracing::warn!("shell full-output persistence failed: {error}");
                    state.full_output_path = None;
                }
            }
        }
        let progress = state.create_progress();
        if let Some(cb) = on_chunk.as_mut() {
            cb("", &|| Some(progress.clone()));
        }
        progress
    };
    // A stalled spool backpressures reads, not child status/kill/deadline.
    let (exec_result, progress) = tokio::join!(execution, consumer);
    // Pull the scalar/move fields out once so we never read a partially-moved
    // `progress` (e.g. `progress.truncation.truncated` after `truncation:` moved it).
    let ShellCaptureProgress {
        output,
        truncation,
        full_output_path,
        last_line_bytes,
    } = progress;
    let truncated_flag = truncation.truncated;
    match exec_result {
        Ok(out) => {
            let cancelled = options.cancel.map(|t| t.is_cancelled()).unwrap_or(false);
            Ok(ShellCaptureResult {
                output,
                truncation,
                full_output_path,
                last_line_bytes,
                exit_code: if cancelled { None } else { Some(out.exit_code) },
                cancelled,
                truncated: truncated_flag,
                execution_error: None,
            })
        }
        Err(e) => {
            let is_aborted = e.code == ExecutionErrorCode::Aborted
                || options.cancel.map(|t| t.is_cancelled()).unwrap_or(false);
            if is_aborted {
                Ok(ShellCaptureResult {
                    output,
                    truncation,
                    full_output_path,
                    last_line_bytes,
                    exit_code: None,
                    cancelled: true,
                    truncated: truncated_flag,
                    execution_error: None,
                })
            } else if options.return_execution_errors {
                Ok(ShellCaptureResult {
                    output,
                    truncation,
                    full_output_path,
                    last_line_bytes,
                    exit_code: None,
                    cancelled: false,
                    truncated: truncated_flag,
                    execution_error: Some(e),
                })
            } else {
                Err(e)
            }
        }
    }
}

async fn append_full_output(
    env: &Arc<dyn ExecutionEnv>,
    path: &std::path::Path,
    writer: &mut Option<Box<dyn crate::env::AppendFile>>,
    buffer: &mut String,
) -> Result<(), ExecutionError> {
    let result = if let Some(writer) = writer {
        writer.append(buffer.as_bytes()).await
    } else {
        env.append_file(
            &path.to_string_lossy(),
            FileContent::Text(std::mem::take(buffer)),
            None,
        )
        .await
    };
    result.map_err(|error| ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string()))?;
    buffer.clear();
    Ok(())
}

/// Persist the prefix and reuse the backend's asynchronous append capability
/// when available; other environments keep their existing append implementation.
async fn init_full_output_file(
    env: &Arc<dyn ExecutionEnv>,
    initial: &str,
) -> Result<(PathBuf, Option<Box<dyn crate::env::AppendFile>>), ExecutionError> {
    let path = env
        .create_temp_file("bash-", ".log", None)
        .await
        .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
    let mut writer = env
        .open_append_file(&path.to_string_lossy())
        .await
        .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
    match &mut writer {
        Some(writer) => writer.append(initial.as_bytes()).await,
        None => {
            env.append_file(
                &path.to_string_lossy(),
                FileContent::Text(initial.to_string()),
                None,
            )
            .await
        }
    }
    .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
    Ok((path, writer))
}

/// Re-export `format_size` for the bash tool's truncation footer messages.
pub use crate::truncate::format_size;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_keeps_tab_lf() {
        let s = "a\x01b\tc\nd\x0be";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "ab\tc\nde");
    }

    #[test]
    fn sanitize_strips_cr() {
        let s = "a\r\nb\rc";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "a\nbc");
    }

    #[test]
    fn sanitize_strips_interlinear_annotation() {
        let s = "a\u{fff9}b\u{fffb}c";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "abc");
    }

    #[test]
    fn sanitize_strips_full_ansi_sequences() {
        // Regression: dropping only the ESC byte used to leave `[36m`-style
        // literal garbage in the captured/output text.
        let s = "\x1b[36mvite v6.4.3\x1b[39m \x1b[32m\u{2713}\x1b[39m built\n";
        assert_eq!(sanitize_and_strip_cr(s), "vite v6.4.3 \u{2713} built\n");
        let s = "a\x1b[1m\x1b[2mX\x1b[22m\x1b[22mb";
        assert_eq!(sanitize_and_strip_cr(s), "aXb");
    }

    #[test]
    fn sanitize_strips_osc_sequences() {
        assert_eq!(sanitize_and_strip_cr("a\x1b]0;title\x07b"), "ab");
        assert_eq!(sanitize_and_strip_cr("a\x1b]8;;http://x\x1b\\b"), "ab");
    }

    #[test]
    fn capture_state_ingest_and_progress() {
        let mut s = CaptureState::default();
        s.ingest_chunk("line1\nline2\n");
        s.ingest_chunk("line3");
        let p = s.create_progress();
        // 3 lines total (line1, line2, line3-open).
        assert_eq!(p.truncation.total_lines, 3);
        assert!(!p.truncation.truncated);
        assert_eq!(p.output, "line1\nline2\nline3");
    }
}
