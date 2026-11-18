//! Ripgrep-backed content search shared by native and virtual filesystems.
//! Pages use logical result offsets and rescan unchanged inputs without a cache.
//! Ordering is breadth-first, case-folded filename order with an exact-name tie
//! break. Content offsets count matching lines; compact offsets count files.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;

use grep_matcher::LineTerminator;
use grep_regex::RegexMatcher;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch,
};
use tokio_util::sync::CancellationToken;

use crate::env::{check_cancel_file, FileKind, FileSystem};
use crate::error::{io_to_file_error, FileError, FileErrorCode};
use crate::truncate::{truncate_line_default, DEFAULT_MAX_BYTES};

// Leave room for bounded tool notices within the public display byte cap.
const MAX_RESULT_BYTES: usize = DEFAULT_MAX_BYTES - 1024;
pub(crate) const MAX_SEARCH_LIMIT: usize = 10_000;
pub(crate) const MAX_SEARCH_CONTEXT: usize = 1_000;

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ContentSearchMode {
    #[default]
    Content,
    Files,
    Count,
}

/// Compiled once per tool call, not once per file.
pub struct ContentSearch {
    pub matcher: RegexMatcher,
    pub glob: Option<(globset::GlobMatcher, bool)>,
    pub context: usize,
    pub limit: usize,
    pub mode: ContentSearchMode,
    pub offset: u64,
}

#[derive(Default)]
pub struct ContentSearchResult {
    pub lines: Vec<String>,
    /// Matching lines actually displayed (files mode counts first-match witnesses).
    pub matches: u64,
    pub returned_results: usize,
    /// Observed callbacks, including offset skips and a possible lookahead.
    /// This is not a total match count when a file or search stops early.
    pub scanned_matches: u64,
    pub next_offset: Option<u64>,
    pub lines_truncated: bool,
    pub byte_limit_reached: bool,
    pub partial_counts: bool,
    pub bytes: usize,
    position: u64,
}

impl ContentSearchResult {
    fn stopped(&self) -> bool {
        self.next_offset.is_some()
    }

    fn push(&mut self, line: String) -> io::Result<bool> {
        let needed = line.len() + usize::from(!self.lines.is_empty());
        if needed > MAX_RESULT_BYTES.saturating_sub(self.bytes) {
            self.byte_limit_reached = true;
            if self.lines.is_empty() && line.len() > MAX_RESULT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a grep result path exceeds the output byte limit",
                ));
            }
            return Ok(false);
        }
        self.bytes += needed;
        self.lines.push(line);
        Ok(true)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.position = self.position.checked_add(1).ok_or_else(counter_overflow)?;
        Ok(())
    }
}

fn counter_overflow() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "grep result counter overflow")
}

impl ContentSearch {
    fn validate(&self) -> Result<(), FileError> {
        if self.limit == 0
            || self.limit > MAX_SEARCH_LIMIT
            || self.context > MAX_SEARCH_CONTEXT
            || self.offset.checked_add(self.limit as u64).is_none()
        {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Invalid grep page bounds",
            ));
        }
        Ok(())
    }

    fn searcher(&self) -> Searcher {
        let context = if self.mode == ContentSearchMode::Content {
            self.context
        } else {
            0
        };
        SearcherBuilder::new()
            .line_number(true)
            .line_terminator(LineTerminator::crlf())
            .before_context(context)
            .after_context(context)
            .binary_detection(BinaryDetection::quit(b'\0'))
            .build()
    }

    fn includes(&self, path: &Path, rel: &str) -> bool {
        match &self.glob {
            Some((matcher, true)) => matcher.is_match(rel),
            Some((matcher, false)) => {
                matcher.is_match(path.file_name().unwrap_or(path.as_os_str()))
            }
            None => true,
        }
    }
}

struct SearchSink<'a> {
    result: &'a mut ContentSearchResult,
    search: &'a ContentSearch,
    rel: &'a str,
    cancel: Option<&'a CancellationToken>,
    last_match: u64,
    started: bool,
    file_matches: u64,
    pending_context: VecDeque<(u64, String)>,
    pending_bytes: usize,
}

impl<'a> SearchSink<'a> {
    fn new(
        result: &'a mut ContentSearchResult,
        search: &'a ContentSearch,
        rel: &'a str,
        cancel: Option<&'a CancellationToken>,
    ) -> Self {
        Self {
            result,
            search,
            rel,
            cancel,
            last_match: 0,
            started: false,
            file_matches: 0,
            pending_context: VecDeque::new(),
            pending_bytes: 0,
        }
    }

    fn render(&mut self, number: u64, bytes: &[u8], separator: char) -> String {
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        let line = truncate_line_default(&text);
        self.result.lines_truncated |= line.was_truncated;
        format!("{}{separator}{number}{separator} {}", self.rel, line.text)
    }

    fn omit(&mut self) -> bool {
        self.result.next_offset = Some(self.result.position);
        false
    }

    fn content_match(&mut self, mat: &SinkMatch<'_>) -> io::Result<bool> {
        let number = mat.line_number().unwrap_or(1);
        if self.result.position < self.search.offset {
            self.result.advance()?;
            self.pending_context.clear();
            self.pending_bytes = 0;
            return Ok(true);
        }
        let at_limit = self.result.returned_results >= self.search.limit;
        if at_limit
            && (!self.started
                || number > self.last_match.saturating_add(self.search.context as u64))
        {
            return Ok(self.omit());
        }
        // Preserve trailing context, including matching lines. Such displayed
        // matching lines advance the cursor too, so continuation never repeats
        // them as new matches on the next page.
        let output = self.render(number, mat.bytes(), if at_limit { '-' } else { ':' });
        if !self.started {
            // Only the first selected match's before-context belongs here,
            // not a distant skipped match's after-context.
            while self
                .pending_context
                .front()
                .is_some_and(|(line, _)| line.saturating_add(self.search.context as u64) < number)
            {
                if let Some((_, line)) = self.pending_context.pop_front() {
                    self.pending_bytes -= line.len() + 1;
                }
            }
            // Context must not consume the space needed for its actual match.
            let available = MAX_RESULT_BYTES
                .saturating_sub(self.result.bytes)
                .saturating_sub(output.len() + 1);
            while self.pending_bytes > available {
                if let Some((_, line)) = self.pending_context.pop_front() {
                    self.pending_bytes -= line.len() + 1;
                    self.result.byte_limit_reached = true;
                } else {
                    break;
                }
            }
            for (_, line) in self.pending_context.drain(..) {
                self.result.push(line)?;
            }
            self.pending_bytes = 0;
        }
        if !self.result.push(output)? {
            return Ok(self.omit());
        }
        self.started = true;
        self.result.matches = self
            .result
            .matches
            .checked_add(1)
            .ok_or_else(counter_overflow)?;
        self.result.advance()?;
        if !at_limit {
            self.result.returned_results += 1;
            self.last_match = number;
        }
        Ok(true)
    }
}

impl Sink for SearchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        check_cancel_io(self.cancel)?;
        self.result.scanned_matches = self
            .result
            .scanned_matches
            .checked_add(1)
            .ok_or_else(counter_overflow)?;
        if self.search.mode == ContentSearchMode::Content {
            return self.content_match(mat);
        }
        if self.file_matches == 0 {
            if self.result.position < self.search.offset {
                self.result.advance()?;
                return Ok(false);
            }
            if self.result.returned_results >= self.search.limit {
                return Ok(self.omit());
            }
            if self.search.mode == ContentSearchMode::Files {
                if !self.result.push(self.rel.to_owned())? {
                    return Ok(self.omit());
                }
                self.result.matches = self
                    .result
                    .matches
                    .checked_add(1)
                    .ok_or_else(counter_overflow)?;
                self.result.returned_results += 1;
                self.result.advance()?;
                // Filename identity comes from the backend, never rendered text.
                return Ok(false);
            }
        }
        self.file_matches = self
            .file_matches
            .checked_add(1)
            .ok_or_else(counter_overflow)?;
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        check_cancel_io(self.cancel)?;
        if self.result.position < self.search.offset {
            return Ok(true);
        }
        let number = context.line_number().unwrap_or(1);
        if self.result.returned_results >= self.search.limit
            && (!self.started
                || number > self.last_match.saturating_add(self.search.context as u64))
        {
            return Ok(true);
        }
        let line = self.render(number, context.bytes(), '-');
        if self.started {
            // Omitted context is not a logical result; the next actual match
            // establishes continuation if it cannot fit.
            self.result.push(line)?;
        } else {
            self.pending_bytes += line.len() + 1;
            self.pending_context.push_back((number, line));
            while self.pending_bytes > MAX_RESULT_BYTES {
                if let Some((_, line)) = self.pending_context.pop_front() {
                    self.pending_bytes -= line.len() + 1;
                    self.result.byte_limit_reached = true;
                }
            }
        }
        Ok(true)
    }

    fn finish(&mut self, _: &Searcher, finish: &SinkFinish) -> io::Result<()> {
        check_cancel_io(self.cancel)?;
        if self.search.mode == ContentSearchMode::Count && self.file_matches != 0 {
            let partial = finish.binary_byte_offset().is_some();
            let suffix = if partial {
                " (partial: binary data)"
            } else {
                ""
            };
            let line = format!("{}: {}{suffix}", self.rel, self.file_matches);
            if !self.result.push(line)? {
                self.omit();
                return Ok(());
            }
            self.result.partial_counts |= partial;
            self.result.matches = self
                .result
                .matches
                .checked_add(self.file_matches)
                .ok_or_else(counter_overflow)?;
            self.result.returned_results += 1;
            self.result.advance()?;
        }
        Ok(())
    }
}

fn check_cancel_io(cancel: Option<&CancellationToken>) -> io::Result<()> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        Err(io::Error::other("operation aborted"))
    } else {
        Ok(())
    }
}

struct CancelReader<'a, R> {
    inner: R,
    cancel: Option<&'a CancellationToken>,
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check_cancel_io(self.cancel)?;
        self.inner.read(bytes)
    }
}

/// Native files stay streaming, with one reusable ripgrep search buffer. Walk
/// one directory at a time so a result limit never requires a full-tree scan.
pub(crate) fn search_native(
    path: &Path,
    search: ContentSearch,
    cancel: Option<&CancellationToken>,
) -> Result<ContentSearchResult, FileError> {
    check_cancel_file(cancel, None)?;
    search.validate()?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| io_to_file_error(error, Some(&path.to_string_lossy())))?;
    let mut result = ContentSearchResult::default();
    let mut searcher = search.searcher();
    let mut search_file = |path: &Path, rel: &str| {
        if search.includes(path, rel) {
            if let Ok(file) = std::fs::File::open(path) {
                let reader = CancelReader {
                    inner: file,
                    cancel,
                };
                let sink = SearchSink::new(&mut result, &search, rel, cancel);
                searcher
                    .search_reader(&search.matcher, reader, sink)
                    .map_err(|error| io_to_file_error(error, Some(rel)))?;
            }
        }
        check_cancel_file(cancel, None)?;
        Ok::<_, FileError>(result.stopped())
    };
    if metadata.is_file() {
        let rel = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        search_file(path, &rel)?;
    } else if metadata.is_dir() {
        let mut queue = VecDeque::from([path.to_path_buf()]);
        while let Some(directory) = queue.pop_front() {
            check_cancel_file(cancel, None)?;
            let walker = ignore::WalkBuilder::new(&directory)
                .hidden(false)
                .max_depth(Some(1))
                .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
                .build();
            let mut entries = Vec::new();
            for entry in walker {
                check_cancel_file(cancel, None)?;
                if let Ok(entry) = entry {
                    if entry.depth() != 0 {
                        entries.push(entry);
                    }
                }
            }
            entries.sort_by_cached_key(|entry| {
                (
                    entry.file_name().to_string_lossy().to_lowercase(),
                    entry.file_name().to_owned(),
                )
            });
            for entry in entries {
                match entry.file_type() {
                    Some(kind) if kind.is_dir() => queue.push_back(entry.into_path()),
                    Some(kind) if kind.is_file() => {
                        let rel = entry
                            .path()
                            .strip_prefix(path)
                            .unwrap_or(entry.path())
                            .to_string_lossy();
                        if search_file(entry.path(), &rel)? {
                            return Ok(result);
                        }
                    }
                    _ => {}
                }
            }
        }
    } else {
        return Err(FileError::new(
            FileErrorCode::Invalid,
            "Path is neither a file nor a directory",
        ));
    }
    Ok(result)
}

/// Virtual backends supply bytes, but use exactly the same ripgrep engine.
pub(crate) async fn search_env<E: FileSystem + ?Sized>(
    env: &E,
    path: &str,
    search: ContentSearch,
    cancel: Option<&CancellationToken>,
) -> Result<ContentSearchResult, FileError> {
    check_cancel_file(cancel, Some(path))?;
    search.validate()?;
    let mut kind = env.file_info(path, cancel).await?.kind;
    if kind == FileKind::Symlink {
        let canonical = env.canonical_path(path, cancel).await?;
        kind = env
            .file_info(&canonical.to_string_lossy(), cancel)
            .await?
            .kind;
    }
    if !matches!(kind, FileKind::File | FileKind::Directory) {
        return Err(FileError::new(
            FileErrorCode::Invalid,
            "Path is neither a file nor a directory",
        ));
    }
    let mut result = ContentSearchResult::default();
    let mut searcher = search.searcher();
    let root = Path::new(path);
    let mut queue = VecDeque::from([(root.to_path_buf(), kind)]);
    while let Some((path, kind)) = queue.pop_front() {
        check_cancel_file(cancel, None)?;
        if kind == FileKind::Directory {
            let mut entries = match env.list_dir(&path.to_string_lossy(), cancel).await {
                Ok(entries) => entries,
                Err(error) if error.code == FileErrorCode::Aborted => return Err(error),
                Err(_) => continue,
            };
            entries.sort_by_cached_key(|entry| (entry.name.to_lowercase(), entry.name.clone()));
            // Search this directory's files before descending into children.
            for entry in entries
                .iter()
                .filter(|entry| entry.kind == FileKind::File)
                .rev()
            {
                queue.push_front((entry.path.clone(), entry.kind));
            }
            for entry in entries
                .into_iter()
                .filter(|entry| entry.kind == FileKind::Directory && entry.name != ".git")
            {
                queue.push_back((entry.path, entry.kind));
            }
            continue;
        }
        let rel = if path == root {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        } else {
            path.strip_prefix(root).unwrap_or(&path).to_string_lossy()
        };
        if search.includes(&path, &rel) {
            match env.read_binary_file(&path.to_string_lossy(), cancel).await {
                Ok(bytes) => {
                    let sink = SearchSink::new(&mut result, &search, &rel, cancel);
                    searcher
                        .search_slice(&search.matcher, &bytes, sink)
                        .map_err(|error| io_to_file_error(error, Some(&rel)))?;
                }
                Err(error) if error.code == FileErrorCode::Aborted => return Err(error),
                Err(_) => {}
            }
        }
        if result.stopped() {
            break;
        }
    }
    check_cancel_file(cancel, Some(&root.to_string_lossy()))?;
    Ok(result)
}
