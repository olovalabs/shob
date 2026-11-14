//! Fail-closed policy for the explicitly selected experimental print backend.
//! Kept compiled without the feature so its reserved flag cannot become an
//! unknown extension flag followed by a silent JSONL fallback.
use crate::args::{Args, Mode, RunMode};
use std::path::{Component, Path, PathBuf};

pub(crate) const FLAG: &str = "--experimental-native-store";
fn flag_present(argv: &[String], flag: &str) -> bool {
    let remote = argv.iter().enumerate().any(|(index, arg)| {
        arg.starts_with("--connect=") || (arg == "--connect" && index + 1 < argv.len())
    });
    let mut tokens = argv.iter();
    while let Some(arg) = tokens.next() {
        if arg == flag
            || arg
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
        {
            return true;
        }
        // The early remote dispatcher consumes these values unconditionally,
        // including flag-shaped tokens. Do not reinterpret a credential/address.
        if remote && matches!(arg.as_str(), "--token" | "--connect") {
            tokens.next();
        }
    }
    false
}
pub(crate) fn early_check(argv: &[String]) -> Result<(), String> {
    if !flag_present(argv, FLAG) {
        return Ok(());
    }
    if matches!(
        argv.first().map(String::as_str),
        Some(
            "__rpi_dev_cleanup"
                | "dev"
                | "dev-local"
                | "auth"
                | "events"
                | "update"
                | "install"
                | "uninstall"
                | "extension"
                | "resource"
                | "package"
                | "pi-package"
                | "pi-update"
                | "install-pi"
                | "uninstall-pi"
        )
    ) || flag_present(argv, "--connect")
    {
        return Err(format!("{FLAG} is only available for a fresh --print invocation, not a subcommand or --connect"));
    }
    Ok(())
}
pub(crate) fn validate(args: &Args) -> Result<(), String> {
    let Some(root) = &args.experimental_native_store else {
        return Ok(());
    };
    if !cfg!(feature = "experimental-native-store") {
        return Err(format!(
            "{FLAG} requires a binary built with the experimental-native-store Cargo feature"
        ));
    }
    if !args.errors.is_empty() {
        return Err(format!(
            "{FLAG} cannot create a session from invalid arguments"
        ));
    }
    if root.as_os_str().is_empty() || root.to_string_lossy().trim().is_empty() {
        return Err(format!("{FLAG} requires a non-empty dedicated directory"));
    }
    if !args.print || !matches!(args.mode, Mode::Text | Mode::Json) {
        return Err(format!(
            "{FLAG} requires explicit --print with text or json mode"
        ));
    }
    if args.continue_session
        || args.resume
        || args.session.is_some()
        || args.session_id.is_some()
        || args.fork.is_some()
        || args.no_session
        || args.session_dir.is_some()
        || args.export.is_some()
        || args.list_models.is_some()
        || ["server", "web", "connect"]
            .iter()
            .any(|key| args.unknown_flags.contains_key(*key))
    {
        return Err(format!("{FLAG} cannot be combined with session selection, --session-dir, export, list-models, RPC, server, Web or remote modes"));
    }
    if std::env::var("RPI_FORCE_TUI").ok().as_deref() == Some("1") {
        return Err(format!("{FLAG} cannot be combined with RPI_FORCE_TUI=1"));
    }
    if root.is_relative()
        && (root.has_root() || root.components().any(|c| matches!(c, Component::Prefix(_))))
    {
        return Err(format!("{FLAG} directory must be absolute or relative to the launch cwd, not drive/root-relative"));
    }
    Ok(())
}
pub(crate) fn root(args: &Args, cwd: &Path) -> Result<Option<PathBuf>, String> {
    validate(args)?;
    args.experimental_native_store
        .as_ref()
        .map(|root| {
            let root = if root.is_absolute() {
                root.clone()
            } else {
                cwd.join(root)
            };
            if !root.is_absolute() {
                return Err(format!("{FLAG} requires an absolute launch cwd"));
            }
            if root.is_file() {
                return Err(format!("{FLAG} directory is a file"));
            }
            Ok(root)
        })
        .transpose()
}
pub(crate) fn validate_stdio(selected: bool, owns_stdio: bool) -> Result<(), String> {
    if selected && owns_stdio {
        Err(format!("{FLAG} does not support stdio-owning extensions"))
    } else {
        Ok(())
    }
}
pub(crate) fn validate_mode(selected: bool, mode: RunMode) -> Result<(), String> {
    if selected && !matches!(mode, RunMode::Print | RunMode::Json) {
        Err(format!("{FLAG} cannot enter interactive or RPC mode"))
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests;
