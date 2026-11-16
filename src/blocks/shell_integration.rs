//! Injects Shob's shell hooks without touching the user's dotfiles.
//!
//! zsh gets a private `ZDOTDIR` whose rc files source the user's originals and
//! then our hook. bash gets a generated `--rcfile`. fish gets an
//! `--init-command`. The scripts are written once to the user cache directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use portable_pty::CommandBuilder;

const ZSH_SCRIPT: &str = include_str!("../../assets/shell-integration/shob.zsh");
const BASH_SCRIPT: &str = include_str!("../../assets/shell-integration/shob.bash");
const FISH_SCRIPT: &str = include_str!("../../assets/shell-integration/shob.fish");

/// Random per-run token stamped into Shob's OSC 133 markers. fish and agent
/// TUIs emit their own 133 sequences; only our hooks know this token.
pub fn shell_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{:x}{:x}", std::process::id(), nanos)
    })
}

fn integration_root() -> Option<&'static PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| install().ok()).as_ref()
}

fn install() -> std::io::Result<PathBuf> {
    let cache_dir = directories::BaseDirs::new()
        .map(|dirs| dirs.cache_dir().to_path_buf())
        .ok_or_else(|| std::io::Error::other("no cache directory"))?;
    let root = cache_dir.join("shob").join("shell-integration");

    let zsh_dir = root.join("zsh");
    let bash_dir = root.join("bash");
    let fish_dir = root.join("fish");
    fs::create_dir_all(&zsh_dir)?;
    fs::create_dir_all(&bash_dir)?;
    fs::create_dir_all(&fish_dir)?;

    fs::write(root.join("shob.zsh"), ZSH_SCRIPT)?;
    fs::write(root.join("shob.bash"), BASH_SCRIPT)?;
    fs::write(root.join("shob.fish"), FISH_SCRIPT)?;

    // zsh reads .zshenv/.zshrc from $ZDOTDIR. Our copies source the user's
    // originals first, then hand control to our hook.
    fs::write(
        zsh_dir.join(".zshenv"),
        r#"SHOB_INTEGRATION_DIR="${ZDOTDIR:-$HOME}"
if [ -f "${SHOB_USER_ZDOTDIR:-$HOME}/.zshenv" ]; then
  source "${SHOB_USER_ZDOTDIR:-$HOME}/.zshenv"
fi
ZDOTDIR="$SHOB_INTEGRATION_DIR"
"#,
    )?;
    fs::write(
        zsh_dir.join(".zshrc"),
        r#"if [ -f "${SHOB_USER_ZDOTDIR:-$HOME}/.zshrc" ]; then
  source "${SHOB_USER_ZDOTDIR:-$HOME}/.zshrc"
fi
if [ -f "$SHOB_INTEGRATION_DIR/shob.zsh" ]; then
  source "$SHOB_INTEGRATION_DIR/shob.zsh"
fi
"#,
    )?;

    fs::write(
        bash_dir.join("bashrc"),
        format!(
            r#"if [ -f "$HOME/.bashrc" ]; then
  source "$HOME/.bashrc"
fi
if [ -f "{}" ]; then
  source "{}"
fi
"#,
            root.join("shob.bash").display(),
            root.join("shob.bash").display()
        ),
    )?;

    fs::write(fish_dir.join("shob.fish"), FISH_SCRIPT)?;

    Ok(root)
}

fn user_zdotdir() -> Option<String> {
    std::env::var("ZDOTDIR")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|value| !value.is_empty()))
}

/// Configure a shell command to load Shob's hooks.
///
/// A user can opt out with `SHOB_NO_SHELL_INTEGRATION=1`; unknown shells are
/// left untouched and the pane stays in classic terminal mode.
pub fn configure_shell(cmd: &mut CommandBuilder, shell_name: &str) {
    if std::env::var_os("SHOB_NO_SHELL_INTEGRATION").is_some() {
        return;
    }
    let Some(root) = integration_root() else {
        return;
    };
    cmd.env("SHOB_SHELL_TOKEN", shell_token());
    configure_with_root(cmd, shell_name, root);
}

fn configure_with_root(cmd: &mut CommandBuilder, shell_name: &str, root: &Path) {
    match shell_name {
        "zsh" => {
            cmd.env(
                "SHOB_USER_ZDOTDIR",
                user_zdotdir().unwrap_or_else(|| "/".to_string()),
            );
            cmd.env("SHOB_INTEGRATION_DIR", root.join("zsh"));
            cmd.env("ZDOTDIR", root.join("zsh"));
        }
        "bash" => {
            cmd.arg("--rcfile");
            cmd.arg(root.join("bash").join("bashrc"));
        }
        "fish" => {
            let script = root.join("fish").join("shob.fish");
            cmd.arg("--init-command");
            cmd.arg(format!("source '{}'", script.display()));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> CommandBuilder {
        CommandBuilder::new("/bin/sh")
    }

    #[test]
    fn zsh_uses_a_private_zdotdir() {
        let root = Path::new("/tmp/shob-test");
        let mut cmd = cmd();
        configure_with_root(&mut cmd, "zsh", root);
        assert_eq!(
            cmd.get_env("ZDOTDIR").and_then(|v| v.to_str()),
            Some("/tmp/shob-test/zsh")
        );
        assert!(cmd.get_env("SHOB_INTEGRATION_DIR").is_some());
        assert_eq!(cmd.get_argv().len(), 1);
    }

    #[test]
    fn bash_gets_an_rcfile() {
        let root = Path::new("/tmp/shob-test");
        let mut cmd = cmd();
        configure_with_root(&mut cmd, "bash", root);
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[1], "--rcfile");
        assert!(argv[2].ends_with("bash/bashrc"));
    }

    #[test]
    fn fish_gets_an_init_command() {
        let root = Path::new("/tmp/shob-test");
        let mut cmd = cmd();
        configure_with_root(&mut cmd, "fish", root);
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[1], "--init-command");
        assert!(argv[2].contains("shob.fish"));
    }

    #[test]
    fn unknown_shell_is_untouched() {
        let root = Path::new("/tmp/shob-test");
        let mut cmd = cmd();
        configure_with_root(&mut cmd, "nu", root);
        assert_eq!(cmd.get_argv().len(), 1);
    }

    #[test]
    fn scripts_are_embedded() {
        assert!(ZSH_SCRIPT.contains("133;A"));
        assert!(BASH_SCRIPT.contains("133;C"));
        assert!(FISH_SCRIPT.contains("fish_preexec"));
    }
}
