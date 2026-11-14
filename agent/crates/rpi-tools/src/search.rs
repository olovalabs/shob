//! Ripgrep-backed content search shared by native and virtual filesystems.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;

use grep_matcher::LineTerminator;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use tokio_util::sync::CancellationToken;

use crate::env::{check_cancel_file, FileKind, FileSystem};
use crate::error::{io_to_file_error, FileError, FileErrorCode};
use crate::truncate::{truncate_line_default, DEFAULT_MAX_BYTES};

/// Compiled once per tool call, not once per file.
pub struct ContentSearch {
    pub matcher: RegexMatcher,
    pub glob: Option<(globset::GlobMatcher, bool)>,
    pub context: usize,
    pub limit: usize,
}

#[derive(Default)]
pub struct ContentSearchResult {
    pub lines: Vec<String>,
    pub matches: usize,
    pub lines_truncated: bool,
    bytes: usize,
}

impl ContentSearchResult {
    fn stopped(&self, limit: usize) -> bool {
        self.matches >= limit || self.bytes > DEFAULT_MAX_BYTES
    }
}

impl ContentSearch {
    fn searcher(&self) -> Searcher {
        SearcherBuilder::new()
            .line_number(true)
            .line_terminator(LineTerminator::crlf())
            .before_context(self.context)
            .after_context(self.context)
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
}

impl SearchSink<'_> {
    fn emit(&mut self, number: u64, bytes: &[u8], separator: char) -> io::Result<bool> {
        check_cancel_io(self.cancel)?;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        let line = truncate_line_default(&text);
        self.result.lines_truncated |= line.was_truncated;
        let output = format!("{}{separator}{number}{separator} {}", self.rel, line.text);
        self.result.bytes += output.len() + usize::from(!self.result.lines.is_empty());
        self.result.lines.push(output);
        Ok(self.result.bytes <= DEFAULT_MAX_BYTES)
    }

    fn within_context(&self, number: u64) -> bool {
        self.result.matches < self.search.limit
            || number < self.last_match.saturating_add(self.search.context as u64)
    }
}

impl Sink for SearchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        let number = mat.line_number().unwrap_or(1);
        let separator = if self.result.matches < self.search.limit {
            self.result.matches += 1;
            self.last_match = number;
            ':'
        } else {
            // Matching lines inside the final match's trailing context remain
            // context, rather than exceeding the tool's global match limit.
            if number > self.last_match.saturating_add(self.search.context as u64) {
                return Ok(false);
            }
            '-'
        };
        Ok(self.emit(number, mat.bytes(), separator)? && self.within_context(number))
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        let number = context.line_number().unwrap_or(1);
        if self.result.matches >= self.search.limit
            && number > self.last_match.saturating_add(self.search.context as u64)
        {
            return Ok(false);
        }
        Ok(self.emit(number, context.bytes(), '-')? && self.within_context(number))
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
                let sink = SearchSink {
                    result: &mut result,
                    search: &search,
                    rel,
                    cancel,
                    last_match: 0,
                };
                searcher
                    .search_reader(&search.matcher, reader, sink)
                    .map_err(|error| io_to_file_error(error, Some(rel)))?;
            }
        }
        check_cancel_file(cancel, None)?;
        Ok::<_, FileError>(result.stopped(search.limit))
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
            entries.sort_by_cached_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
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
            entries.sort_by_cached_key(|entry| entry.name.to_lowercase());
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
                    let sink = SearchSink {
                        result: &mut result,
                        search: &search,
                        rel: &rel,
                        cancel,
                        last_match: 0,
                    };
                    searcher
                        .search_slice(&search.matcher, &bytes, sink)
                        .map_err(|error| io_to_file_error(error, Some(&rel)))?;
                }
                Err(error) if error.code == FileErrorCode::Aborted => return Err(error),
                Err(_) => {}
            }
        }
        if result.stopped(search.limit) {
            break;
        }
    }
    check_cancel_file(cancel, Some(&root.to_string_lossy()))?;
    Ok(result)
}
