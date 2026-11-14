//! Native loader gate for the externally built official extension set.
//!
//! This is intentionally ignored during ordinary workspace tests. CI invokes it
//! after building and staging the `rpi-package` catalog, so a missing or ABI
//! incompatible cdylib cannot be silently skipped by the production loader.

use rpi_extensions::{load_session, PluginDiagnostics};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingDiagnostics {
    warnings: Mutex<Vec<String>>,
}

impl PluginDiagnostics for RecordingDiagnostics {
    fn warn(&self, message: &str) {
        self.warnings
            .lock()
            .expect("diagnostic lock")
            .push(message.into());
    }

    fn unsupported(&self, message: &str) {
        self.warn(message);
    }
}

fn is_cdylib(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("dll" | "dylib" | "so")
    )
}

#[test]
#[ignore = "requires the externally built rpi-package extension catalog"]
fn every_staged_official_extension_loads_without_skipping() {
    let directory = std::env::var_os("RPI_SMOKE_PLUGIN_DIR")
        .map(PathBuf::from)
        .expect("RPI_SMOKE_PLUGIN_DIR must point to the staged extension catalog");
    let expected: Vec<_> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_cdylib(path))
        .collect();
    assert!(
        !expected.is_empty(),
        "no staged cdylibs in {}",
        directory.display()
    );

    let diagnostics = Arc::new(RecordingDiagnostics::default());
    let session = load_session(&[directory.clone()], diagnostics.clone(), None);
    let expected_names: BTreeSet<_> = expected
        .iter()
        .filter_map(|path| path.file_name())
        .collect();
    let loaded_names: BTreeSet<_> = session
        .loaded_paths()
        .iter()
        .filter_map(|path| path.file_name())
        .collect();
    assert_eq!(
        loaded_names,
        expected_names,
        "the native loader did not load exactly the staged extensions: {:?}",
        diagnostics.warnings.lock().expect("diagnostic lock")
    );

    let failures: Vec<_> = diagnostics
        .warnings
        .lock()
        .expect("diagnostic lock")
        .iter()
        .filter(|message| {
            message.contains("skipped plugin")
                || message.contains("unreadable")
                || message.contains("ABI")
        })
        .cloned()
        .collect();
    assert!(
        failures.is_empty(),
        "extension load diagnostics: {failures:?}"
    );
    assert!(
        session.snapshot().is_some(),
        "the staged extension set registered no native snapshot"
    );
}
