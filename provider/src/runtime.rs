//! Location of historical provider transcripts.

use std::path::PathBuf;

pub fn root() -> PathBuf {
    let data = std::env::var_os("KATHAMO_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = directories::BaseDirs::new()?;
            Some(if cfg!(target_os = "macos") {
                dirs.home_dir().join("Library/Application Support/Kathamo")
            } else if cfg!(windows) {
                dirs.data_local_dir().join("Kathamo")
            } else {
                dirs.data_local_dir().join("kathamo")
            })
        })
        .unwrap_or_else(|| PathBuf::from(".kathamo"));
    std::path::absolute(data.join("provider")).expect("resolve Kathamo provider data directory")
}

pub fn agent_dir() -> PathBuf {
    root().join("agent")
}
