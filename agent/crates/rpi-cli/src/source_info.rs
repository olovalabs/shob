//! Resource provenance metadata.
//!
//! Port of the upstream `packages/coding-agent/src/core/source-info.ts`. Every
//! discovered resource (extension, skill, prompt, theme, package file) can carry
//! where it came from: the owning package source, the scope it applies to, and
//! whether it is a top-level file or package-contributed. The UI uses this to
//! explain *why* a given skill or command is present.

use std::path::PathBuf;

/// Where a resource applies (native `SourceScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

impl SourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Temporary => "temporary",
        }
    }
}

/// Whether a resource came from a package or sits at the top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

impl SourceOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::TopLevel => "top-level",
        }
    }
}

/// Provenance for a discovered resource (native `SourceInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub path: PathBuf,
    /// e.g. `npm:foo`, `git:…`, `local`.
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<PathBuf>,
}

impl SourceInfo {
    /// Construct from explicit fields (native `createSourceInfo`).
    pub fn new(
        path: impl Into<PathBuf>,
        source: impl Into<String>,
        scope: SourceScope,
        origin: SourceOrigin,
        base_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
            scope,
            origin,
            base_dir,
        }
    }

    /// Construct a synthetic source (native `createSyntheticSourceInfo`):
    /// defaults to `temporary` scope and `top-level` origin.
    pub fn synthetic(path: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: None,
        }
    }

    /// A short `source · scope` label for UI display.
    pub fn label(&self) -> String {
        format!("{} · {}", self.source, self.scope.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_defaults() {
        let info = SourceInfo::synthetic("/x/SKILL.md", "local");
        assert_eq!(info.scope, SourceScope::Temporary);
        assert_eq!(info.origin, SourceOrigin::TopLevel);
        assert_eq!(info.label(), "local · temporary");
    }

    #[test]
    fn explicit_fields() {
        let info = SourceInfo::new(
            "/p/SKILL.md",
            "npm:foo",
            SourceScope::Project,
            SourceOrigin::Package,
            Some(PathBuf::from("/p")),
        );
        assert_eq!(info.origin.as_str(), "package");
        assert_eq!(info.base_dir, Some(PathBuf::from("/p")));
    }
}
