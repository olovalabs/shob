//! Location of historical provider transcripts.

use std::path::PathBuf;

pub fn root() -> PathBuf {
    let data = std::env::var_os("SHOB_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = directories::BaseDirs::new()?;
            Some(if cfg!(target_os = "macos") {
                dirs.home_dir().join("Library/Application Support/Shob")
            } else if cfg!(windows) {
                dirs.data_local_dir().join("Shob")
            } else {
                dirs.data_local_dir().join("shob")
            })
        })
        .unwrap_or_else(|| PathBuf::from(".shob"));
    std::path::absolute(data.join("provider")).expect("resolve Shob provider data directory")
}

pub fn agent_dir() -> PathBuf {
    root().join("agent")
}
