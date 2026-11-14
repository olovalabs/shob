//! Trust management for project resources.
//!
//! Provides functionality to manage trust decisions for project-local resources,
//! extensions, and configurations. This ensures that untrusted projects cannot
//! execute arbitrary code or load malicious resources.
//!
//! # Overview
//!
//! When a project is not trusted, the following restrictions apply:
//! - Project-local extensions are not loaded
//! - Project-local skills and prompts are not loaded
//! - Project-local context files are not loaded
//! - Certain settings are ignored
//!
//! Trust decisions are persisted in `.rpi/trust.json` and can be managed via
//! the `/trust` slash command or the `--approve`/`--no-approve` CLI flags.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Trust decision for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum TrustDecision {
    /// Project is trusted; load all project-local resources.
    Trusted,
    /// Project is not trusted; restrict project-local resources.
    NotTrusted,
    /// No decision has been made yet; prompt the user.
    #[default]
    Undecided,
}

/// Trust configuration for a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectTrust {
    /// The project path (normalized).
    pub path: String,
    /// The trust decision.
    pub decision: TrustDecision,
    /// When the decision was made (Unix timestamp in milliseconds).
    pub decided_at: Option<i64>,
    /// Who made the decision (user, auto, etc.).
    pub decided_by: Option<String>,
}

/// Trust store that manages trust decisions for multiple projects.
#[derive(Debug, Clone)]
pub struct TrustStore {
    /// Map from normalized project path to trust decision.
    decisions: Arc<RwLock<HashMap<String, ProjectTrust>>>,
    /// Path to the trust store file.
    store_path: PathBuf,
}

impl TrustStore {
    /// Create a new trust store.
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            decisions: Arc::new(RwLock::new(HashMap::new())),
            store_path,
        }
    }

    /// Load trust decisions from disk.
    pub fn load(&self) -> Result<(), TrustError> {
        if !self.store_path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.store_path)
            .map_err(|e| TrustError::IoError(e.to_string()))?;

        let decisions: HashMap<String, ProjectTrust> =
            serde_json::from_str(&content).map_err(|e| TrustError::ParseError(e.to_string()))?;

        let mut store = self.decisions.write().map_err(|_| TrustError::LockError)?;
        *store = decisions;

        Ok(())
    }

    /// Save trust decisions to disk.
    pub fn save(&self) -> Result<(), TrustError> {
        let decisions = self.decisions.read().map_err(|_| TrustError::LockError)?;
        let content = serde_json::to_string_pretty(&*decisions)
            .map_err(|e| TrustError::SerializeError(e.to_string()))?;

        // Create parent directories if they don't exist
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TrustError::IoError(e.to_string()))?;
        }

        std::fs::write(&self.store_path, content)
            .map_err(|e| TrustError::IoError(e.to_string()))?;

        Ok(())
    }

    /// Get the trust decision for a project.
    pub fn get_decision(&self, project_path: &Path) -> TrustDecision {
        let normalized = normalize_path(project_path);
        let decisions = self.decisions.read().unwrap_or_else(|e| e.into_inner());

        decisions
            .get(&normalized)
            .map(|t| t.decision)
            .unwrap_or(TrustDecision::Undecided)
    }

    /// Set the trust decision for a project.
    pub fn set_decision(
        &self,
        project_path: &Path,
        decision: TrustDecision,
        decided_by: Option<String>,
    ) -> Result<(), TrustError> {
        let normalized = normalize_path(project_path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let trust = ProjectTrust {
            path: normalized.clone(),
            decision,
            decided_at: Some(now),
            decided_by,
        };

        let mut decisions = self.decisions.write().map_err(|_| TrustError::LockError)?;
        decisions.insert(normalized, trust);

        Ok(())
    }

    /// Check if a project is trusted.
    pub fn is_trusted(&self, project_path: &Path) -> bool {
        self.get_decision(project_path) == TrustDecision::Trusted
    }

    /// Remove the trust decision for a project.
    pub fn remove_decision(&self, project_path: &Path) -> Result<(), TrustError> {
        let normalized = normalize_path(project_path);
        let mut decisions = self.decisions.write().map_err(|_| TrustError::LockError)?;
        decisions.remove(&normalized);
        Ok(())
    }

    /// Get all trust decisions.
    pub fn all_decisions(&self) -> Vec<ProjectTrust> {
        let decisions = self.decisions.read().unwrap_or_else(|e| e.into_inner());
        decisions.values().cloned().collect()
    }
}

/// Normalize a project path for consistent lookup.
fn normalize_path(path: &Path) -> String {
    // Convert to absolute path and normalize
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    // Convert to string and normalize separators
    absolute
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

/// Errors that can occur during trust operations.
#[derive(Debug, Clone)]
pub enum TrustError {
    /// I/O error reading or writing the trust store.
    IoError(String),
    /// Error parsing the trust store file.
    ParseError(String),
    /// Error serializing the trust store.
    SerializeError(String),
    /// Lock error (poisoned mutex).
    LockError,
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::IoError(e) => write!(f, "I/O error: {}", e),
            TrustError::ParseError(e) => write!(f, "Parse error: {}", e),
            TrustError::SerializeError(e) => write!(f, "Serialize error: {}", e),
            TrustError::LockError => write!(f, "Lock error"),
        }
    }
}

impl std::error::Error for TrustError {}

/// Check if a project should be trusted based on various heuristics.
///
/// This function implements automatic trust detection based on:
/// - Whether the project is in a known safe directory (e.g., home directory)
/// - Whether the project has a `.rpi/trust.json` file with a positive decision
/// - Whether the user has explicitly approved the project via CLI flag
pub fn should_trust_project(
    project_path: &Path,
    trust_store: &TrustStore,
    cli_approve: Option<bool>,
) -> bool {
    // CLI flag takes precedence
    if let Some(approve) = cli_approve {
        return approve;
    }

    // Check trust store
    match trust_store.get_decision(project_path) {
        TrustDecision::Trusted => return true,
        TrustDecision::NotTrusted => return false,
        TrustDecision::Undecided => {}
    }

    // Auto-trust projects in home directory
    if let Some(home) = dirs::home_dir() {
        if project_path.starts_with(&home) {
            return true;
        }
    }

    // Default to not trusted for undecided projects
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_trust_decision_default() {
        assert_eq!(TrustDecision::default(), TrustDecision::Undecided);
    }

    #[test]
    fn test_trust_store_new() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("trust.json");
        let store = TrustStore::new(store_path);

        assert_eq!(store.all_decisions().len(), 0);
    }

    #[test]
    fn test_trust_store_set_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("trust.json");
        let store = TrustStore::new(store_path);

        let project_path = Path::new("/test/project");

        // Initially undecided
        assert_eq!(store.get_decision(project_path), TrustDecision::Undecided);

        // Set to trusted
        store
            .set_decision(
                project_path,
                TrustDecision::Trusted,
                Some("test".to_string()),
            )
            .unwrap();
        assert_eq!(store.get_decision(project_path), TrustDecision::Trusted);
        assert!(store.is_trusted(project_path));

        // Set to not trusted
        store
            .set_decision(project_path, TrustDecision::NotTrusted, None)
            .unwrap();
        assert_eq!(store.get_decision(project_path), TrustDecision::NotTrusted);
        assert!(!store.is_trusted(project_path));
    }

    #[test]
    fn test_trust_store_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("trust.json");

        // Create and populate store
        let store1 = TrustStore::new(store_path.clone());
        let project1 = Path::new("/test/project1");
        let project2 = Path::new("/test/project2");

        store1
            .set_decision(project1, TrustDecision::Trusted, Some("user".to_string()))
            .unwrap();
        store1
            .set_decision(project2, TrustDecision::NotTrusted, None)
            .unwrap();
        store1.save().unwrap();

        // Load into new store
        let store2 = TrustStore::new(store_path);
        store2.load().unwrap();

        assert_eq!(store2.get_decision(project1), TrustDecision::Trusted);
        assert_eq!(store2.get_decision(project2), TrustDecision::NotTrusted);
    }

    #[test]
    fn test_normalize_path() {
        let path1 = Path::new("/test/project");
        let path2 = Path::new("/test/project/");

        assert_eq!(normalize_path(path1), normalize_path(path2));
    }

    #[test]
    fn test_should_trust_project_with_cli_flag() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("trust.json");
        let store = TrustStore::new(store_path);
        let project_path = Path::new("/test/project");

        // CLI flag overrides everything
        assert!(should_trust_project(project_path, &store, Some(true)));
        assert!(!should_trust_project(project_path, &store, Some(false)));
    }

    #[test]
    fn test_should_trust_project_with_store() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("trust.json");
        let store = TrustStore::new(store_path);
        let project_path = Path::new("/test/project");

        // Store decision overrides default
        store
            .set_decision(project_path, TrustDecision::Trusted, None)
            .unwrap();
        assert!(should_trust_project(project_path, &store, None));

        store
            .set_decision(project_path, TrustDecision::NotTrusted, None)
            .unwrap();
        assert!(!should_trust_project(project_path, &store, None));
    }
}
