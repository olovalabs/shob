//! Lightweight code syntax highlighting for terminal rendering.
//!
//! Upstream highlights fenced code with highlight.js
//! (`utils/syntax-highlight.ts`). rpi ships no JS highlighter, so this is a
//! small, dependency-free tokenizer covering the languages that show up most in
//! agent transcripts (Rust, JS/TS, Python, Go, JSON, shell, C-family, Ruby).
//!
//! It highlights four token classes — comments, strings, numbers and
//! keywords — and leaves everything else in the code-block body color. It does
//! **not** aim for highlight.js parity; unknown languages and unclassifiable
//! lines simply render in the base color, which is the correct fallback.

use crate::theme::ThemeColors;

/// Highlight state that persists across lines (block comments).
#[derive(Debug, Clone, Default)]
pub struct Highlighter {
    in_block_comment: bool,
    profile: Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Profile {
    #[default]
    Plain,
    SlashSlash, // rust, js, ts, go, c-family, java
    Hash,       // python, ruby, shell, yaml, toml
    Json,
    Sql,
}

impl Profile {
    fn for_language(language: &str) -> Self {
        match language {
            "rust" | "rs" | "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "go"
            | "golang" | "c" | "cpp" | "c++" | "h" | "hpp" | "java" | "csharp" | "cs" | "swift"
            | "kotlin" | "scala" | "dart" | "php" => Self::SlashSlash,
            "python" | "py" | "ruby" | "rb" | "bash" | "sh" | "shell" | "zsh" | "fish" | "yaml"
            | "yml" | "toml" | "ini" | "r" | "perl" | "pl" | "makefile" | "dockerfile" => {
                Self::Hash
            }
            "json" | "jsonc" => Self::Json,
            "sql" => Self::Sql,
            _ => Self::Plain,
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::SlashSlash => Some("//"),
            Self::Hash => Some("#"),
            Self::Sql => Some("--"),
            // JSON has no comments; JSONC is handled by the caller as SlashSlash.
            Self::Json | Self::Plain => None,
        }
    }

    fn supports_block_comments(self) -> bool {
        matches!(self, Self::SlashSlash | Self::Sql)
    }

    fn hash_is_comment(self) -> bool {
        self == Self::Hash
    }
}

fn keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" | "rs" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while", "crate",
        ],
        "python" | "py" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "not", "or", "pass", "raise", "return", "True", "try",
            "while", "with", "yield",
        ],
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "default",
            "delete",
            "do",
            "else",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "let",
            "new",
            "null",
            "return",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "undefined",
            "var",
            "void",
            "while",
            "yield",
            "interface",
            "type",
            "enum",
            "implements",
            "private",
            "public",
            "readonly",
        ],
        "go" | "golang" => &[
            "break",
            "case",
            "chan",
            "const",
            "continue",
            "default",
            "defer",
            "else",
            "fallthrough",
            "for",
            "func",
            "go",
            "goto",
            "if",
            "import",
            "interface",
            "map",
            "package",
            "range",
            "return",
            "select",
            "struct",
            "switch",
            "type",
            "var",
            "nil",
            "true",
            "false",
        ],
        "bash" | "sh" | "shell" | "zsh" | "fish" => &[
            "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
            "function", "return", "export", "local", "in",
        ],
        "c" | "cpp" | "c++" | "h" | "hpp" | "java" | "csharp" | "cs" => &[
            "auto",
            "bool",
            "break",
            "case",
            "catch",
            "char",
            "class",
            "const",
            "continue",
            "default",
            "do",
            "double",
            "else",
            "enum",
            "extern",
            "false",
            "float",
            "for",
            "if",
            "int",
            "long",
            "namespace",
            "new",
            "private",
            "protected",
            "public",
            "return",
            "short",
            "static",
            "struct",
            "switch",
            "template",
            "this",
            "throw",
            "true",
            "try",
            "typedef",
            "void",
            "while",
            "using",
            "override",
            "virtual",
        ],
        _ => &[
            "true", "false", "null", "if", "else", "for", "while", "return", "function", "class",
        ],
    }
}

impl Highlighter {
    pub fn new(language: &str) -> Self {
        let lang = language.trim().to_ascii_lowercase();
        // `jsonc` allows `//` comments.
        let profile = if lang == "jsonc" {
            Profile::SlashSlash
        } else {
            Profile::for_language(&lang)
        };
        Self {
            in_block_comment: false,
            profile,
        }
    }

    /// Highlight one line, returning an ANSI-styled string. The caller is
    /// responsible for width computation on the *plain* text.
    pub fn highlight(&mut self, line: &str, colors: &ThemeColors) -> String {
        let base = colors.md_code_block;
        let comment = colors.muted;
        let string = colors.success;
        let number = colors.info;
        let keyword = colors.accent;

        if self.profile == Profile::Plain {
            return base.fg(line);
        }

        let keywords = keywords_from_profile(self.profile);
        let mut out = String::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        let mut plain_start = 0usize;

        // Flush `plain_start..i` in base color when a token begins at `i`.
        macro_rules! flush {
            () => {
                if i > plain_start {
                    let text: String = chars[plain_start..i].iter().collect();
                    out.push_str(&base.fg(&text));
                }
            };
        }

        while i < chars.len() {
            // Continue a block comment started on a previous line.
            if self.in_block_comment {
                let close = if self.profile == Profile::Sql {
                    "*/"
                } else {
                    "*/"
                };
                if let Some(off) = find_seq(&chars, i, close) {
                    let end = off + 2;
                    let text: String = chars[i..end].iter().collect();
                    out.push_str(&comment.fg(&text));
                    i = end;
                    self.in_block_comment = false;
                    plain_start = i;
                    continue;
                } else {
                    let text: String = chars[i..].iter().collect();
                    out.push_str(&comment.fg(&text));
                    return finish(&out);
                }
            }

            let rest: String = chars[i..].iter().collect();
            let c = chars[i];

            // Line comment.
            if let Some(marker) = self.profile.line_comment() {
                if rest.starts_with(marker) {
                    flush!();
                    out.push_str(&comment.fg(&rest));
                    return finish(&out);
                }
            }
            // `#` line comment (python/shell/ruby) — only at line start or after
            // whitespace so `#{...}` interpolation and shebangs are not mangled
            // beyond the comment rule (shebang is a comment anyway).
            if self.profile.hash_is_comment()
                && c == '#'
                && (i == 0 || chars[i - 1].is_whitespace())
            {
                flush!();
                out.push_str(&comment.fg(&rest));
                return finish(&out);
            }

            // Block comment start.
            if self.profile.supports_block_comments() && rest.starts_with("/*") {
                flush!();
                if let Some(off) = find_seq(&chars, i + 2, "*/") {
                    let end = off + 2;
                    let text: String = chars[i..end].iter().collect();
                    out.push_str(&comment.fg(&text));
                    i = end;
                    plain_start = i;
                    continue;
                } else {
                    let text: String = chars[i..].iter().collect();
                    out.push_str(&comment.fg(&text));
                    self.in_block_comment = true;
                    return finish(&out);
                }
            }

            // String literal.
            if c == '"' || c == '\'' || c == '`' {
                flush!();
                let quote = c;
                let mut j = i + 1;
                let mut escaped = false;
                while j < chars.len() {
                    let cj = chars[j];
                    if escaped {
                        escaped = false;
                    } else if cj == '\\' {
                        escaped = true;
                    } else if cj == quote {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                let text: String = chars[i..j.min(chars.len())].iter().collect();
                out.push_str(&string.fg(&text));
                i = j.min(chars.len());
                plain_start = i;
                continue;
            }

            // Number.
            if c.is_ascii_digit() && (i == 0 || !is_ident_char(chars[i - 1])) {
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
                {
                    j += 1;
                }
                flush!();
                let text: String = chars[i..j].iter().collect();
                out.push_str(&number.fg(&text));
                i = j;
                plain_start = i;
                continue;
            }

            // Identifier / keyword.
            if is_ident_start(c) {
                let mut j = i;
                while j < chars.len() && is_ident_char(chars[j]) {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                if keywords.contains(&word.as_str()) {
                    flush!();
                    out.push_str(&keyword.fg(&word));
                    i = j;
                    plain_start = i;
                    continue;
                }
                i = j;
                continue;
            }

            i += 1;
        }
        flush!();
        finish(&out)
    }
}

fn keywords_from_profile(profile: Profile) -> &'static [&'static str] {
    match profile {
        Profile::SlashSlash => keywords("rust"),
        Profile::Hash => keywords("python"),
        Profile::Json => &["true", "false", "null"],
        Profile::Sql => &[
            "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE", "CREATE", "TABLE", "JOIN",
            "ON", "GROUP", "BY", "ORDER", "LIMIT", "AND", "OR", "NOT", "NULL", "AS",
        ],
        Profile::Plain => &[],
    }
}

fn find_seq(chars: &[char], from: usize, seq: &str) -> Option<usize> {
    let target: Vec<char> = seq.chars().collect();
    if target.is_empty() || chars.len() < target.len() {
        return None;
    }
    let start = from.min(chars.len());
    for i in start..=chars.len() - target.len() {
        if chars[i..i + target.len()] == target[..] {
            return Some(i);
        }
    }
    None
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn finish(out: &str) -> String {
    // Close any open SGR so the panel background (applied later) is not
    // overridden past the end of the line.
    if out.is_empty() {
        String::new()
    } else {
        format!("{out}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_language_is_passthrough() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("text");
        let out = h.highlight("hello world", &colors);
        assert!(out.contains("hello world"));
    }

    #[test]
    fn rust_keyword_is_highlighted() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("rust");
        let out = h.highlight("fn main() { let x = 1; }", &colors);
        assert!(out.contains("fn"));
        assert!(out.contains("let"));
        // The keyword color is applied at least once.
        assert!(out.contains('\x1b'));
    }

    #[test]
    fn line_comment_is_full_line() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("rust");
        let out = h.highlight("// a comment with fn keyword", &colors);
        // `fn` inside a comment must not have been treated as a keyword.
        assert_eq!(out.matches(colors.accent.to_fg().as_str()).count(), 0);
    }

    #[test]
    fn block_comment_spans_lines() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("rust");
        let first = h.highlight("/* start", &colors);
        assert!(first.contains("start"));
        assert!(h.in_block_comment);
        let second = h.highlight("still comment */ fn x()", &colors);
        assert!(!h.in_block_comment);
        // `fn` after the close is a keyword.
        assert!(second.contains("fn"));
    }

    #[test]
    fn string_literal_swallows_keyword() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("python");
        let out = h.highlight("s = \"if for while\"", &colors);
        assert!(out.contains("if for while"));
    }

    #[test]
    fn json_literals() {
        let colors = ThemeColors::default();
        let mut h = Highlighter::new("json");
        let out = h.highlight("{\"a\": true, \"b\": null}", &colors);
        assert!(out.contains("true"));
    }
}
