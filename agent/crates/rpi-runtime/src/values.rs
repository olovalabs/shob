//! Explicit environment and secret-command access supplied by the host.
use std::collections::BTreeMap;

/// Freeze environment, cached commands and time for one resolution. Uncached
/// header commands retain their existing per-expansion semantics.
pub(crate) struct ResolutionValues<'a> {
    source: &'a dyn ValueSource,
    environment: std::sync::Mutex<BTreeMap<String, Option<String>>>,
    commands: std::sync::Mutex<BTreeMap<String, Option<String>>>,
    now: i64,
}
impl<'a> ResolutionValues<'a> {
    pub(crate) fn new(source: &'a dyn ValueSource) -> Self {
        Self {
            source,
            environment: Default::default(),
            commands: Default::default(),
            now: source.now_unix_seconds(),
        }
    }
}
impl ValueSource for ResolutionValues<'_> {
    fn env(&self, name: &str) -> Option<String> {
        self.environment
            .lock()
            .unwrap()
            .entry(name.into())
            .or_insert_with(|| self.source.env(name))
            .clone()
    }
    fn command(&self, command: &str, cached: bool) -> Option<String> {
        if !cached {
            return self.source.command(command, false);
        }
        self.commands
            .lock()
            .unwrap()
            .entry(command.into())
            .or_insert_with(|| self.source.command(command, true))
            .clone()
    }
    fn now_unix_seconds(&self) -> i64 {
        self.now
    }
}

/// Hosts choose which environment values and commands the runtime may access.
/// Commands are disabled by default; the library never reads process env itself.
pub trait ValueSource: Send + Sync {
    fn env(&self, name: &str) -> Option<String>;
    fn command(&self, _command: &str, _cached: bool) -> Option<String> {
        None
    }
    fn now_unix_seconds(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(i64::MAX)
    }
}
impl ValueSource for BTreeMap<String, String> {
    fn env(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}
pub fn resolve_config_value(
    raw: &str,
    overlay: Option<&BTreeMap<String, String>>,
    values: &dyn ValueSource,
    cached: bool,
) -> Option<String> {
    match raw.strip_prefix('!') {
        Some(command) => values.command(command, cached),
        None => resolve_template(raw, overlay, values),
    }
}
pub fn resolve_headers(
    headers: &BTreeMap<String, String>,
    overlay: Option<&BTreeMap<String, String>>,
    values: &dyn ValueSource,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            resolve_config_value(value, overlay, values, false).map(|value| (name.clone(), value))
        })
        .collect()
}
/// A parsed template part — literal text or an env-var reference.
enum TemplatePart {
    Literal(String),
    Env(String),
}

/// Parse a `$VAR`/`${VAR}` template (mirrors pi `parseConfigValueTemplate`).
/// `$$`→`$` and `$!`→`!` are escapes; `${NAME}` requires `NAME` to match
/// `^[A-Za-z_][A-Za-z0-9_]*$` else the raw slice is kept literal; `$NAME` takes
/// the longest `[A-Za-z_][A-Za-z0-9_]*` prefix as the name.
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let mut parts: Vec<TemplatePart> = Vec::new();
    let bytes = config.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Find the next `$`.
        match config[i..].find('$') {
            None => {
                push_literal(&mut parts, &config[i..]);
                break;
            }
            Some(offset) => {
                let dollar = i + offset;
                push_literal(&mut parts, &config[i..dollar]);
                let after = dollar + 1;
                let next = bytes.get(after).copied();
                if next == Some(b'$') || next == Some(b'!') {
                    push_literal(&mut parts, &config[after..after + 1]);
                    i = after + 1;
                    continue;
                }
                if next == Some(b'{') {
                    // ${NAME}
                    if let Some(end_rel) = config[after + 1..].find('}') {
                        let end = after + 1 + end_rel;
                        let name = &config[after + 1..end];
                        if is_env_name(name) {
                            parts.push(TemplatePart::Env(name.to_string()));
                        } else {
                            // Not a valid name — keep the raw `${…}` literal.
                            push_literal(&mut parts, &config[dollar..=end]);
                        }
                        i = end + 1;
                        continue;
                    }
                    // No closing `}` — literal `$`.
                    push_literal(&mut parts, "$");
                    i = after;
                    continue;
                }
                // $NAME (greedy prefix). Bare `$` with no name char follows.
                if let Some(name) = env_name_prefix(&config[after..]) {
                    parts.push(TemplatePart::Env(name.to_string()));
                    i = after + name.len();
                } else {
                    push_literal(&mut parts, "$");
                    i = after;
                }
            }
        }
    }
    parts
}

fn push_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(s)) = parts.last_mut() {
        s.push_str(value);
    } else {
        parts.push(TemplatePart::Literal(value.to_string()));
    }
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The longest `[A-Za-z_][A-Za-z0-9_]*` prefix of `s` (mirrors the TS
/// `ENV_VAR_NAME_PREFIX_RE` match), or `None` when `s` doesn't start with one.
fn env_name_prefix(s: &str) -> Option<&str> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = chars
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    Some(&s[..end])
}

/// Resolve a parsed template: any referenced env var that is unset ⇒ the whole
/// value is `None` (pi semantics). Literal-only templates pass through as-is.
pub fn resolve_template(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
    values: &dyn ValueSource,
) -> Option<String> {
    let parts = parse_template(config);
    let mut out = String::with_capacity(config.len());
    for part in parts {
        match part {
            TemplatePart::Literal(s) => out.push_str(&s),
            TemplatePart::Env(name) => {
                let v = env_overlay
                    .and_then(|overlay| overlay.get(&name).cloned())
                    .or_else(|| values.env(&name))?;
                out.push_str(&v)
            }
        }
    }
    Some(out)
}
