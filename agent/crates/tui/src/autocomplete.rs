//! Autocomplete support for TUI.
//!
//! Provides autocomplete suggestions for file paths, commands, and custom items.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::fuzzy::fuzzy_filter;

/// Path delimiters for parsing.
const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

/// Greatest char-boundary byte index in `s` that is `<= idx` (clamped to
/// `s.len()`). Callers receive a `cursor` that may be a *character* count or a
/// byte offset handed back from the editor; either way, slicing
/// `&input[..cursor]` panics if `cursor` lands inside a multibyte char. Snap to
/// the nearest preceding boundary so the slice is always sound.
///
/// A position that IS a char boundary (including the END of the string) is
/// kept as-is — the old `take_while(b <= idx)` returned the START of the char
/// at `idx`, so `end: cursor` came out one char short and Tab-accept kept the
/// input's last char: typing "/mode" + Tab produced "/modele" instead of
/// "/model".
fn snap_cursor(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    if s.is_char_boundary(idx) {
        return idx;
    }
    s.char_indices()
        .take_while(|(b, _)| *b < idx)
        .last()
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// An autocomplete item.
#[derive(Debug, Clone)]
pub struct AutocompleteItem {
    /// Display text.
    pub text: String,
    /// Display label (optional, defaults to text).
    pub label: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Whether this is a directory.
    pub is_directory: bool,
    /// Whether to insert a space after this item.
    pub insert_space: bool,
}

impl AutocompleteItem {
    /// Create a new autocomplete item.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            label: None,
            description: None,
            is_directory: false,
            insert_space: true,
        }
    }

    /// Add a label.
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_string());
        self
    }

    /// Add a description.
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Mark as directory.
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }

    /// Set whether to insert space after.
    pub fn with_insert_space(mut self, insert_space: bool) -> Self {
        self.insert_space = insert_space;
        self
    }

    /// Get the display text.
    pub fn display_text(&self) -> &str {
        self.label.as_ref().unwrap_or(&self.text)
    }
}

/// Autocomplete suggestions result.
#[derive(Debug, Clone)]
pub struct AutocompleteSuggestions {
    /// List of suggestions.
    pub items: Vec<AutocompleteItem>,
    /// Start position in the input.
    pub start: usize,
    /// End position in the input.
    pub end: usize,
    /// Whether this is a path completion.
    pub is_path: bool,
}

/// Autocomplete provider trait.
pub trait AutocompleteProvider: Send + Sync {
    /// Get suggestions for the given input.
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions>;

    /// Get the name of this provider.
    fn name(&self) -> &str;
}

/// Slash command for command completion.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Command name (including the slash).
    pub name: String,
    /// Description.
    pub description: String,
}

/// File path autocomplete provider.
pub struct FilePathAutocompleteProvider {
    root_path: Option<PathBuf>,
    max_results: usize,
}

impl FilePathAutocompleteProvider {
    /// Create a new file path provider.
    pub fn new() -> Self {
        Self {
            root_path: None,
            max_results: 50,
        }
    }

    /// Create a provider with a root path.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root_path: Some(root),
            max_results: 50,
        }
    }

    /// Set max results.
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    /// Extract the path prefix from input.
    fn extract_path_prefix(&self, input: &str, cursor: usize) -> Option<(String, usize)> {
        // Snap to a char boundary so the slices below can never land inside a
        // multibyte char (callers may pass a char count, not a byte offset).
        let cursor = snap_cursor(input, cursor);
        // Find the start of the current path token
        let before_cursor = &input[..cursor];

        // Find the last path delimiter before cursor
        let mut start = 0;
        for (i, c) in before_cursor.char_indices().rev() {
            if PATH_DELIMITERS.contains(&c) {
                start = i + 1;
                break;
            }
        }

        // Check for @file reference
        let token = &input[start..cursor];
        if token.starts_with('@') {
            // Return start AFTER the @ so the @ is preserved in text[..start]
            return Some((token[1..].to_string(), start + 1));
        } else if token.starts_with('"') {
            // Quoted path
            return Some((token[1..].to_string(), start + 1));
        }

        // Native pi's `extractPathPrefix` only hands a token to the file
        // provider when it actually looks like a path: a separator anywhere
        // in the token, or a leading `.` / `~/`. A bare word in prose (`c`,
        // `hhhh`) must not trigger a cwd `read_dir` on every caret move.
        // The `@` branch above is the explicit opt-in; a token right after
        // a `"` keeps the port's pre-existing quoted-path channel (`"` is
        // itself a delimiter, so `start` points past it).
        let looks_like_path = token.contains('/')
            || token.contains('\\')
            || token.starts_with('.')
            || token.starts_with("~/");
        let quoted = start > 0 && input[..start].ends_with('"');
        if !looks_like_path && !quoted {
            return None;
        }

        Some((token.to_string(), start))
    }

    /// Get completions for a path prefix.
    fn get_path_completions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let mut items = Vec::new();

        // Determine the directory to search
        let (dir, partial) = if prefix.ends_with(['/', '\\']) {
            // A prefix that ENDS at a separator (`…/nested/`) asks for the
            // contents of that directory. `Path::parent`/`file_name` ignore a
            // trailing separator, so splitting there would search the parent
            // for an entry named after the directory itself: the text
            // `…/nested/` would be offered `nested/` again, and inserting it
            // manufactures `…/nested/nested/`, a path that does not exist.
            (PathBuf::from(prefix), String::new())
        } else if prefix.contains('/') || prefix.contains('\\') {
            let path = Path::new(prefix);
            let parent = path.parent().unwrap_or(Path::new("."));
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (parent.to_path_buf(), file_name.to_string())
        } else {
            (
                self.root_path.clone().unwrap_or_else(|| PathBuf::from(".")),
                prefix.to_string(),
            )
        };

        // Read directory
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.take(self.max_results) {
                if let Ok(entry) = entry {
                    let name = entry.file_name().to_string_lossy().to_string();

                    // Filter by prefix
                    if !partial.is_empty()
                        && !name.to_lowercase().starts_with(&partial.to_lowercase())
                    {
                        continue;
                    }

                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let text = if is_dir { format!("{}/", name) } else { name };

                    items.push(
                        AutocompleteItem::new(&text)
                            .as_directory()
                            .with_insert_space(!is_dir),
                    );
                }
            }
        }

        items
    }
}

impl Default for FilePathAutocompleteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AutocompleteProvider for FilePathAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        let cursor = snap_cursor(input, cursor);
        let (prefix, start) = self.extract_path_prefix(input, cursor)?;

        if prefix.is_empty() && !input[start..cursor].starts_with('@') {
            return None;
        }

        let items = self.get_path_completions(&prefix);
        if items.is_empty() {
            return None;
        }

        Some(AutocompleteSuggestions {
            items,
            start,
            end: cursor,
            is_path: true,
        })
    }

    fn name(&self) -> &str {
        "filepath"
    }
}

/// Slash command autocomplete provider.
pub struct SlashCommandAutocompleteProvider {
    commands: Vec<SlashCommand>,
}

impl SlashCommandAutocompleteProvider {
    /// Create a new slash command provider.
    pub fn new(commands: Vec<SlashCommand>) -> Self {
        Self { commands }
    }

    /// Create with default commands plus a set of dynamically-discovered slash
    /// commands. The discovered commands (e.g. prompt-template names exposed as
    /// `/expand`-style `/`-prefixed entries) are appended to the built-in set so
    /// the built-in names win on a fuzzy tie and the discovered names appear as
    /// additional suggestions. Mirrors pi's behavior where prompt-template
    /// invocations (`/<name>`) surface alongside built-in slash commands in
    /// `/`-autocomplete (`agent-session.ts:1124` + `expandPromptTemplate`).
    pub fn with_commands(discovered: Vec<SlashCommand>) -> Self {
        let mut commands = Self::with_default_commands().commands;
        commands.extend(discovered);
        Self { commands }
    }

    /// Create with default commands.
    pub fn with_default_commands() -> Self {
        Self::new(vec![
            SlashCommand {
                name: "/help".to_string(),
                description: "Show help information".to_string(),
            },
            SlashCommand {
                name: "/clear".to_string(),
                description: "Clear the conversation".to_string(),
            },
            SlashCommand {
                name: "/exit".to_string(),
                description: "Exit the application".to_string(),
            },
            SlashCommand {
                name: "/model".to_string(),
                description: "Change the model".to_string(),
            },
            SlashCommand {
                name: "/context".to_string(),
                description: "List discovered context files, skills, and prompt templates"
                    .to_string(),
            },
        ])
    }

    /// Add a command.
    pub fn add_command(&mut self, command: SlashCommand) {
        self.commands.push(command);
    }
}

impl AutocompleteProvider for SlashCommandAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        let cursor = snap_cursor(input, cursor);
        let before_cursor = &input[..cursor];

        // Check if we're at the start of a slash command
        if !before_cursor.starts_with('/') {
            return None;
        }

        let query = &before_cursor[1..];
        let items: Vec<AutocompleteItem> = fuzzy_filter(&self.commands, query, |cmd| &cmd.name)
            .into_iter()
            .map(|cmd| {
                AutocompleteItem::new(&cmd.name)
                    .with_description(&cmd.description)
                    .with_insert_space(false)
            })
            .collect();

        if items.is_empty() {
            return None;
        }

        Some(AutocompleteSuggestions {
            items,
            start: 0,
            end: cursor,
            is_path: false,
        })
    }

    fn name(&self) -> &str {
        "slash_command"
    }
}

/// Combined autocomplete provider that chains multiple providers.
pub struct CombinedAutocompleteProvider {
    providers: Vec<Arc<dyn AutocompleteProvider>>,
}

impl CombinedAutocompleteProvider {
    /// Create a new combined provider.
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Add a provider.
    pub fn add_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        self.providers.push(provider);
    }

    /// Create with default providers.
    pub fn with_defaults() -> Self {
        let mut combined = Self::new();
        combined.add_provider(Arc::new(
            SlashCommandAutocompleteProvider::with_default_commands(),
        ));
        combined.add_provider(Arc::new(FilePathAutocompleteProvider::new()));
        combined
    }
}

impl Default for CombinedAutocompleteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        for provider in &self.providers {
            if let Some(suggestions) = provider.get_suggestions(input, cursor) {
                return Some(suggestions);
            }
        }
        None
    }

    fn name(&self) -> &str {
        "combined"
    }
}

/// Global autocomplete manager.
pub struct AutocompleteManager {
    provider: Mutex<Option<Arc<dyn AutocompleteProvider>>>,
}

impl AutocompleteManager {
    /// Create a new manager.
    pub fn new() -> Self {
        Self {
            provider: Mutex::new(None),
        }
    }

    /// Set the provider.
    pub fn set_provider(&self, provider: Arc<dyn AutocompleteProvider>) {
        if let Ok(mut p) = self.provider.lock() {
            *p = Some(provider);
        }
    }

    /// Get suggestions.
    pub fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        self.provider
            .lock()
            .ok()?
            .as_ref()?
            .get_suggestions(input, cursor)
    }
}

impl Default for AutocompleteManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory for a filesystem probe; removed on drop.
    /// The provider tests live in this crate, which has no `tempfile`
    /// dev-dependency — a per-test unique name under the OS temp dir is
    /// enough and keeps `Cargo.lock` untouched.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("rpi-tui-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snap_cursor_keeps_boundaries_including_end() {
        // Regression: the caret at the END of the input (what Tab-accept sees)
        // snapped one char short, so `end` excluded the last char and accept
        // kept it: "/mode" + Tab became "/modele" instead of "/model".
        assert_eq!(snap_cursor("/mode", 5), 5);
        assert_eq!(snap_cursor("/mo", 3), 3);
        assert_eq!(snap_cursor("/model", 6), 6);
        assert_eq!(snap_cursor("abc", 3), 3);
        // Interior boundary + mid-multibyte snap down.
        assert_eq!(snap_cursor("/mode", 2), 2);
        assert_eq!(snap_cursor("你", 1), 0);
        assert_eq!(snap_cursor("你", 3), 3);
        // Out-of-range clamps to the end.
        assert_eq!(snap_cursor("/mode", 99), 5);
    }
    #[test]
    fn test_slash_command_provider() {
        let provider = SlashCommandAutocompleteProvider::with_default_commands();
        let suggestions = provider.get_suggestions("/he", 3);

        assert!(suggestions.is_some());
        let s = suggestions.unwrap();
        assert!(!s.items.is_empty());
        assert!(s.items[0].text.starts_with("/help"));
    }

    #[test]
    fn test_autocomplete_item() {
        let item = AutocompleteItem::new("test.txt")
            .with_description("A test file")
            .as_directory();

        assert_eq!(item.text, "test.txt");
        assert!(item.is_directory);
    }

    /// A prefix that ends at a separator asks for the directory's CONTENTS.
    /// `Path::parent`/`file_name` ignore the trailing separator, so splitting
    /// there used to search the parent for an entry named after the directory
    /// itself and offer `nested/` for the text `…/nested/` — inserting it
    /// again produced `…/nested/nested/`, a path that does not exist.
    #[test]
    fn file_path_provider_browses_into_a_trailing_separator() {
        let scratch = Scratch::new("trailing-separator");
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        assert!(!nested.join("nested").exists());
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let prefix = format!("@{}{}", nested.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&prefix, prefix.len())
            .expect("the directory's contents are a completion");
        let texts: Vec<&str> = suggestions
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["report.txt"],
            "a trailing separator must list the directory, not offer its own name"
        );
        assert_eq!(suggestions.start, 1, "the @ stays outside the span");
        assert_eq!(suggestions.end, prefix.len());
    }

    /// The partial-name path is unchanged: `…/nes` still completes the entry
    /// `nested/` inside its parent.
    #[test]
    fn file_path_provider_still_completes_a_partial_name() {
        let scratch = Scratch::new("partial-name");
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let prefix = format!("@{}{}nes", scratch.0.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&prefix, prefix.len())
            .expect("a partial directory name");
        let texts: Vec<&str> = suggestions
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts, vec!["nested/"]);
        assert_eq!(suggestions.start, 1);
        assert_eq!(suggestions.end, prefix.len());
    }

    /// Fork issue #11: a bare word in prose is not a path. Holding an arrow
    /// key moves the caret through `n n` / `cc cc` / `hhhh hhhh`, and every
    /// keystroke called `refresh_autocomplete`; the old provider answered
    /// each token (`c`, `hhhh`, ...) with a `read_dir` of cwd, so the file
    /// panel flickered open and shut. Native pi keeps an explicit path
    /// appearance gate here — this probe fails on the pre-fix provider.
    #[test]
    fn bare_word_tokens_never_open_the_file_panel() {
        let scratch = Scratch::new("bare-token");
        std::fs::write(scratch.0.join("chromium-proxy-policy.reg"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let text = "n n\ncc cc\nhhhh hhhh";
        // Every caret position inside a bare word, including the `c` in
        // `cc c|c` from the report: the popup must stay closed for the whole
        // arrow-key sweep. `cursor` is a byte offset, as the editor supplies.
        for (cursor, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            if text[..cursor]
                .chars()
                .last()
                .is_some_and(char::is_whitespace)
            {
                continue;
            }
            assert!(
                provider.get_suggestions(text, cursor).is_none(),
                "bare word at byte {cursor} in {text:?} opened a file panel"
            );
        }
        assert!(provider.get_suggestions("n n\ncc cc", 5).is_none());
    }

    #[test]
    fn a_leading_dot_still_looks_like_a_path() {
        let scratch = Scratch::new("leading-dot");
        std::fs::write(scratch.0.join(".env"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        // Native pi treats any leading `.` as a path trigger, including a
        // lone dot in prose. Keep that exemption rather than inventing a
        // stricter gate than upstream.
        let suggestions = provider
            .get_suggestions("hello .", 7)
            .expect("a leading dot still looks like a path");
        assert_eq!(suggestions.start, 6);
        assert_eq!(suggestions.end, 7);
    }

    /// The gate must not take away any of the explicit ways to ask for a
    /// file: `@` references, quoted paths, and path-shaped tokens. Assert on
    /// the extraction step, not `get_suggestions`: a relative path containing
    /// a separator is resolved against the process cwd (pre-existing
    /// `get_path_completions` behavior), so whether it finds an entry is not
    /// a property of this gate.
    #[test]
    fn path_appearance_and_explicit_references_still_complete() {
        let provider = FilePathAutocompleteProvider::new();
        let cases: &[(&str, &str, usize)] = &[
            ("@st", "st", 1),
            ("@nested/", "nested/", 1),
            ("@\"no", "no", 2),
            ("\"no", "no", 1),
            ("./ed", "./ed", 0),
            ("./", "./", 0),
            ("~/", "~/", 0),
            ("a/b", "a/b", 0),
            ("a\\b", "a\\b", 0),
            ("/ab", "/ab", 0),
        ];
        for (input, prefix, start) in cases {
            let extracted = provider.extract_path_prefix(input, input.len());
            assert_eq!(
                extracted.as_ref().map(|(p, s)| (p.as_str(), *s)),
                Some((*prefix, *start)),
                "{input:?} must still be extracted as a path"
            );
        }
    }

    /// End-to-end: an `@` reference and an absolute path still produce
    /// candidates through `get_suggestions` after the gate.
    #[test]
    fn explicit_file_references_still_resolve() {
        let scratch = Scratch::new("path-gate");
        std::fs::write(scratch.0.join("notes.txt"), "fixture").unwrap();
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());

        let at = "@notes";
        let suggestions = provider
            .get_suggestions(at, at.len())
            .expect("an @ reference completes a root entry");
        assert_eq!(suggestions.items[0].text, "notes.txt");
        assert_eq!(suggestions.start, 1);

        let absolute = format!("@{}{}repo", nested.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&absolute, absolute.len())
            .expect("an absolute @ path completes its basename");
        assert_eq!(suggestions.items[0].text, "report.txt");
    }
}
