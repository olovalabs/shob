//! Structured resource diagnostics.
//!
//! Port of upstream's `packages/coding-agent/src/core/diagnostics.ts`.
//! Resource loading can surface soft problems (a skill/prompt/theme name is
//! defined twice, a package fails to parse, …). Upstream keeps these
//! **structured** — in particular a name collision carries both the winning and
//! the losing path so the UI can explain *which* definition won.

use std::collections::HashMap;

/// Kind of resource that can collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Extension,
    Skill,
    Prompt,
    Theme,
}

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
            Self::Theme => "theme",
        }
    }
}

/// Two resources claiming the same name. `winner_path` is the registration that
/// is kept; `loser_path` is the shadowed (dropped) one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCollision {
    pub resource_type: ResourceType,
    /// The colliding name (skill name, command/tool/flag name, prompt name,
    /// theme name).
    pub name: String,
    pub winner_path: String,
    pub loser_path: String,
    /// e.g. `npm:foo`, `git:...`, `local`.
    pub winner_source: Option<String>,
    pub loser_source: Option<String>,
}

/// A single resource-loading diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceDiagnostic {
    Warning {
        message: String,
        path: Option<String>,
    },
    Error {
        message: String,
        path: Option<String>,
    },
    Collision(ResourceCollision),
}

impl ResourceDiagnostic {
    pub fn message(&self) -> String {
        match self {
            Self::Warning { message, .. } | Self::Error { message, .. } => message.clone(),
            Self::Collision(c) => format!("name \"{}\" collision", c.name),
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Warning { path, .. } | Self::Error { path, .. } => path.as_deref(),
            Self::Collision(c) => Some(c.loser_path.as_str()),
        }
    }

    pub fn is_collision(&self) -> bool {
        matches!(self, Self::Collision(_))
    }
}

/// Deduplicate `(name, path)` items by name, **first registration wins**.
///
/// Returns the winning indices (in order) plus a [`ResourceDiagnostic`] per
/// shadowed duplicate. Mirrors native `dedupePrompts`/`dedupeThemes`/
/// skills-dedupe, which all keep the first occurrence and report the rest.
pub fn dedupe_by_name<'a, I>(
    resource_type: ResourceType,
    items: I,
) -> (Vec<usize>, Vec<ResourceDiagnostic>)
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut seen: HashMap<&'a str, (usize, &'a str)> = HashMap::new();
    let mut winners = Vec::new();
    let mut diagnostics = Vec::new();
    for (index, (name, path)) in items.into_iter().enumerate() {
        if let Some((_, winner_path)) = seen.get(name) {
            diagnostics.push(ResourceDiagnostic::Collision(ResourceCollision {
                resource_type,
                name: name.to_string(),
                winner_path: (*winner_path).to_string(),
                loser_path: path.to_string(),
                winner_source: None,
                loser_source: None,
            }));
        } else {
            seen.insert(name, (index, path));
            winners.push(index);
        }
    }
    (winners, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keeps_first_and_reports_collisions() {
        let items = vec![
            ("alpha", "/proj/alpha.md"),
            ("beta", "/proj/beta.md"),
            ("alpha", "/global/alpha.md"),
        ];
        let (winners, diags) = dedupe_by_name(ResourceType::Prompt, items);
        assert_eq!(winners, vec![0, 1]);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            ResourceDiagnostic::Collision(c) => {
                assert_eq!(c.resource_type, ResourceType::Prompt);
                assert_eq!(c.name, "alpha");
                assert_eq!(c.winner_path, "/proj/alpha.md");
                assert_eq!(c.loser_path, "/global/alpha.md");
            }
            other => panic!("expected collision, got {other:?}"),
        }
    }

    #[test]
    fn diagnostic_accessors() {
        let d = ResourceDiagnostic::Collision(ResourceCollision {
            resource_type: ResourceType::Theme,
            name: "dark".into(),
            winner_path: "<builtin>".into(),
            loser_path: "/x/dark.json".into(),
            winner_source: None,
            loser_source: None,
        });
        assert!(d.is_collision());
        assert_eq!(d.message(), "name \"dark\" collision");
        assert_eq!(d.path(), Some("/x/dark.json"));
    }
}
