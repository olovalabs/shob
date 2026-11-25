use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui_updater_core::{
    Asset, DeltaAsset, EngineConfig, OwnershipHints, Release, UpdateEngine, UpdateSource,
    Verification, Version, current_install_root, install,
};
use serde::Deserialize;

pub const RELEASES: &str = "https://github.com/olovalabs/shob/releases";
const API: &str = "https://api.github.com/repos/olovalabs/shob/releases/latest";
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;

#[derive(Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    Current,
    Available(Box<Update>),
    Installing,
    Ready(Ready),
    #[cfg(unix)]
    Restarting,
    External(String),
    Failed(String),
}

impl State {
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::Available(_) | Self::Ready(_) | Self::External(_)
        )
    }
}

pub struct Update {
    root: PathBuf,
    release: Release,
    format: Format,
}

impl Update {
    pub fn version(&self) -> &Version {
        &self.release.version
    }
}

pub struct Ready {
    pub path: PathBuf,
    pub version: Version,
    #[cfg(windows)]
    pub installer_files: Option<tempfile::TempDir>,
    #[cfg(unix)]
    pub app_image: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    LinuxTar,
    AppImage,
    MacTar,
    WindowsExe,
    WindowsMsi,
}

impl Format {
    fn detect(root: &Path, exe: &Path) -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") if root != exe => Some(Self::AppImage),
            ("linux", "x86_64") => Some(Self::LinuxTar),
            ("macos", "aarch64") if root == exe => Some(Self::MacTar),
            ("windows", "x86_64") if windows_msi(root) => Some(Self::WindowsMsi),
            ("windows", "x86_64") => Some(Self::WindowsExe),
            _ => None,
        }
    }

    fn asset(self, version: &Version) -> String {
        match self {
            Self::LinuxTar => "shob-x86_64-unknown-linux-gnu.tar.gz".into(),
            Self::AppImage => format!("shob-{version}-x86_64.AppImage"),
            Self::MacTar => "shob-aarch64-apple-darwin.tar.gz".into(),
            Self::WindowsExe => "shob-x86_64-pc-windows-msvc.exe".into(),
            Self::WindowsMsi => "shob-x86_64-pc-windows-msvc.msi".into(),
        }
    }
}

fn windows_msi(path: &Path) -> bool {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .any(|dir| {
            path.to_string_lossy().eq_ignore_ascii_case(
                &Path::new(&dir)
                    .join("shob")
                    .join("shob.exe")
                    .to_string_lossy(),
            )
        })
}

pub fn enabled() -> bool {
    option_env!("SHOB_OFFICIAL_RELEASE") == Some("1")
        && std::env::var_os("SHOB_DISABLE_RESTORE").is_none()
}

pub(crate) fn tui_notice() -> Option<agent_cli::updates::UpdateNotice> {
    let State::Available(update) = check().ok()? else {
        return None;
    };
    Some(agent_cli::updates::UpdateNotice {
        name: "shob".into(),
        current: env!("CARGO_PKG_VERSION").into(),
        latest: update.version().to_string(),
        command: "shob update".into(),
    })
}

pub(crate) fn run_cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let option = args.next();
    anyhow::ensure!(args.next().is_none(), "usage: shob update [--check]");
    let check_only = match option.as_deref() {
        Some("-h" | "--help") => {
            println!(
                "Usage: shob update [--check]\nInstall the latest stable Shob release, or check without installing."
            );
            return Ok(());
        }
        Some("--check") => true,
        None => false,
        _ => anyhow::bail!("usage: shob update [--check]"),
    };
    anyhow::ensure!(
        enabled(),
        "Self-updates require an official Shob build with updates enabled. Source installs can be rebuilt with `cargo install --path . --locked`."
    );

    match check()? {
        State::Current => println!("Shob {} is up to date.", env!("CARGO_PKG_VERSION")),
        State::External(message) => println!("{message}"),
        State::Available(update) if check_only => {
            println!("Shob {} is available. Run `shob update`.", update.version());
        }
        State::Available(update) => {
            println!("Installing Shob {}…", update.version());
            let ready = apply(*update)?;
            #[cfg(windows)]
            if let Some(files) = ready.installer_files {
                schedule_msi_install(&ready.path)?;
                let _ = files.keep();
                println!(
                    "Windows Installer will apply Shob {} after this command exits.",
                    ready.version
                );
                return Ok(());
            }
            println!(
                "Installed Shob {}. Restart any running Shob sessions to use it.",
                ready.version
            );
        }
        _ => unreachable!("release check returned a UI lifecycle state"),
    }
    Ok(())
}

#[cfg(windows)]
fn schedule_msi_install(script: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let system_root = std::env::var_os("SystemRoot").context("missing Windows SystemRoot")?;
    let powershell =
        PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    Command::new(powershell)
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Wait-Process -Id $env:SHOB_UPDATE_PID -ErrorAction SilentlyContinue; & $env:SHOB_UPDATE_SCRIPT",
        ])
        .env("SHOB_UPDATE_PID", std::process::id().to_string())
        .env("SHOB_UPDATE_SCRIPT", script)
        .spawn()
        .context("could not schedule Windows Installer")?;
    Ok(())
}

pub fn check() -> Result<State> {
    let can_install = enabled();
    #[cfg(target_os = "windows")]
    if can_install {
        remove_leftovers();
    }
    let Some(published) = Published::latest()? else {
        return Ok(State::Current);
    };
    published.update_for(Version::parse(env!("CARGO_PKG_VERSION"))?, can_install)
}

#[derive(Deserialize)]
struct Published {
    tag_name: String,
    assets: Vec<PublishedAsset>,
}

#[derive(Deserialize)]
struct PublishedAsset {
    name: String,
    size: u64,
    digest: Option<String>,
}

impl Published {
    fn update_for(&self, current: Version, can_install: bool) -> Result<State> {
        let version = gpui_updater_core::parse_tag(&self.tag_name)?;
        if !version.pre.is_empty() || !version.cmp_precedence(&current).is_gt() {
            return Ok(State::Current);
        }
        if !can_install {
            return Ok(State::External(format!(
                "Shob {version} is available on GitHub. In-app installation is disabled for this build or session."
            )));
        }

        let root = current_install_root()?;
        let exe = std::env::current_exe()?;
        let owner = install::ownership(&root, &OwnershipHints::new());
        if !owner.is_self_update() || (cfg!(target_os = "linux") && exe.starts_with("/usr/bin")) {
            let message = owner.update_command().map_or_else(
                || format!("Shob {version} is available. Update through your package manager."),
                |command| format!("Shob {version} is available. Update with {command}."),
            );
            return Ok(State::External(message));
        }
        let Some(format) = Format::detect(&root, &exe) else {
            return Ok(State::External(format!(
                "Shob {version} is available on GitHub."
            )));
        };
        let name = format.asset(&version);
        let Some(release) = self.select(&name, current, version.clone()) else {
            return Ok(State::External(format!(
                "Shob {version} is available. The verified update for this package is not published yet."
            )));
        };
        if let Err(error) = install::check_location(&root, &name) {
            return Ok(State::External(format!(
                "Shob {version} is available. {error}"
            )));
        }
        Ok(State::Available(Box::new(Update {
            root,
            release,
            format,
        })))
    }

    fn latest() -> Result<Option<Self>> {
        let response = reqwest::blocking::Client::builder()
            .user_agent(concat!("shob/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .https_only(true)
            .build()?
            .get(API)
            .send()?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut body = Vec::new();
        response
            .error_for_status()?
            .take(MAX_METADATA + 1)
            .read_to_end(&mut body)?;
        anyhow::ensure!(
            body.len() as u64 <= MAX_METADATA,
            "release metadata is too large"
        );
        Ok(Some(serde_json::from_slice(&body)?))
    }

    fn select(&self, name: &str, current: Version, version: Version) -> Option<Release> {
        let (asset, digest) = self.asset(name)?;
        let mut release = Release::new(version.clone(), asset).with_sha256(digest);
        let delta_name = format!("{name}.from-{current}.delta");
        if let Some((asset, digest)) = self.asset(&delta_name) {
            let mut delta = DeltaAsset::new(current, version, asset);
            delta.sha256 = Some(digest.to_ascii_lowercase());
            release.deltas.push(delta);
        }
        Some(release)
    }

    fn asset(&self, name: &str) -> Option<(Asset, &str)> {
        let asset = self.assets.iter().find(|asset| asset.name == name)?;
        let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
        if asset.size == 0
            || asset.size > MAX_PAYLOAD
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        let mut url = url::Url::parse(RELEASES).expect("GitHub releases URL");
        url.path_segments_mut()
            .ok()?
            .extend(["download", &self.tag_name, name]);
        Some((Asset::new(name, url.to_string(), asset.size), digest))
    }
}

// Installation uses the exact release the user accepted, even if GitHub publishes another.
struct Selected(Release);

impl UpdateSource for Selected {
    fn fetch_latest(&self) -> gpui_updater_core::Result<Release> {
        Ok(self.0.clone())
    }
}

pub fn apply(update: Update) -> Result<Ready> {
    let temp = tempfile::Builder::new().prefix("shob-update-").tempdir()?;
    let mut config = EngineConfig::new(Version::parse(env!("CARGO_PKG_VERSION"))?)
        .verification(Verification::Checksum)
        .install_root(&update.root)
        .download_dir(temp.path())
        .max_download_size(MAX_PAYLOAD);
    if update.format == Format::MacTar {
        config.bundle_version = gpui_updater_core::BundleVersion::Ignore;
    }
    if update.format == Format::WindowsMsi {
        config.install_options =
            gpui_updater_core::InstallOptions::new().msi_ui(gpui_updater_core::MsiUi::Basic);
    }
    let engine = UpdateEngine::new(Selected(update.release), config);
    let release = engine.check()?.context("Shob is already up to date")?;
    let artifact = engine.download(&release, |_, _| {})?;
    let path = match update.format {
        Format::LinuxTar | Format::MacTar | Format::AppImage => {
            install_unix_binary(
                artifact.path(),
                &update.root,
                update.format,
                &release.asset.name,
            )?;
            update.root
        }
        Format::WindowsExe | Format::WindowsMsi => engine
            .install(&artifact)?
            .restart_path
            .context("missing restart path")?,
    };
    Ok(Ready {
        path,
        version: release.version,
        #[cfg(windows)]
        installer_files: (update.format == Format::WindowsMsi).then_some(temp),
        #[cfg(unix)]
        app_image: update.format == Format::AppImage,
    })
}

#[cfg(unix)]
fn install_unix_binary(artifact: &Path, root: &Path, format: Format, name: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};

    let staged = tempfile::NamedTempFile::new_in(root.parent().context("missing install folder")?)?;
    if artifact.is_dir() {
        let source = artifact.join(root.file_name().context("missing executable name")?);
        std::fs::copy(source, staged.path())?;
    } else if format == Format::AppImage {
        std::fs::copy(artifact, staged.path())?;
    } else {
        let member = format!("{}/shob", name.trim_end_matches(".tar.gz"));
        anyhow::ensure!(
            Command::new("tar")
                .arg("-xOzf")
                .arg(artifact)
                .arg(member)
                .stdout(Stdio::from(staged.reopen()?))
                .status()?
                .success(),
            "could not extract Shob from the update archive"
        );
    }
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    if format == Format::MacTar {
        anyhow::ensure!(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--strict"])
                .arg(staged.path())
                .status()?
                .success(),
            "the new Shob executable failed code-signature verification"
        );
    }
    staged.as_file().sync_all()?;
    staged.persist(root)?;
    Ok(())
}

#[cfg(not(unix))]
fn install_unix_binary(_: &Path, _: &Path, _: Format, _: &str) -> Result<()> {
    anyhow::bail!("Unix package on a different platform")
}

#[cfg(unix)]
pub fn schedule_restart(path: &Path, app_image: bool) -> Result<()> {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"while kill -0 "$1" 2>/dev/null; do sleep 0.1; done; exec "$2" gui"#,
            "shob-restart",
            &std::process::id().to_string(),
        ])
        .arg(path)
        .process_group(0);
    if app_image {
        for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            command.env_remove(name);
        }
    }
    command
        .spawn()
        .context("could not start the new Shob process")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_leftovers() {
    if let Ok(root) = current_install_root()
        && !windows_msi(&root)
        && install::ownership(&root, &OwnershipHints::new()).is_self_update()
    {
        install::remove_leftovers(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_build_reports_newer_stable_versions_without_offering_installation() {
        let current = Version::new(1, 9, 0);
        let mut published = Published {
            tag_name: "v1.10.0".into(),
            assets: Vec::new(),
        };
        assert!(matches!(
            published.update_for(current.clone(), false).unwrap(),
            State::External(message) if message.contains("1.10.0")
        ));
        for tag in ["v1.8.0", "v1.9.0", "v1.10.0-beta.1"] {
            published.tag_name = tag.into();
            assert!(matches!(
                published.update_for(current.clone(), false).unwrap(),
                State::Current
            ));
        }
    }

    #[test]
    fn selects_only_the_exact_asset_and_verified_delta_for_the_current_version() {
        let current = Version::new(1, 9, 0);
        let version = Version::new(1, 10, 0);
        let name = Format::AppImage.asset(&version);
        let asset = |name: String, verified: bool| PublishedAsset {
            name,
            size: 500,
            digest: verified.then(|| format!("sha256:{}", "a".repeat(64))),
        };
        let mut published = Published {
            tag_name: "v1.10.0".into(),
            assets: vec![
                asset(format!("{name}.sha256"), true),
                asset(format!("{name}.from-1.8.0.delta"), true),
                asset(format!("{name}.from-1.9.0.delta"), true),
                asset(name.clone(), false),
            ],
        };
        assert!(
            published
                .select(&name, current.clone(), version.clone())
                .is_none()
        );
        *published.assets.last_mut().unwrap() = asset(name.clone(), true);
        let release = published
            .select(&name, current.clone(), version.clone())
            .unwrap();
        assert_eq!(release.asset.name, name);
        assert_eq!(
            release.asset.url,
            format!("{RELEASES}/download/v1.10.0/{name}")
        );
        assert_eq!(release.deltas.len(), 1);
        assert_eq!(release.deltas[0].from, current);
        assert_eq!(release.deltas[0].to, version);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portable_update_replaces_only_the_executable_and_keeps_it_executable() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let name = Format::LinuxTar.asset(&Version::new(1, 0, 0));
        let package = dir.path().join(name.trim_end_matches(".tar.gz"));
        fs::create_dir(&package).unwrap();
        fs::write(package.join("shob"), b"new").unwrap();
        let archive = dir.path().join(&name);
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(dir.path())
                .arg(package.file_name().unwrap())
                .status()
                .unwrap()
                .success()
        );

        let installed = dir.path().join("renamed-shob");
        fs::write(&installed, b"old").unwrap();
        let sibling = dir.path().join("README.md");
        fs::write(&sibling, b"original readme").unwrap();
        assert!(
            install_unix_binary(&archive, &installed, Format::LinuxTar, "wrong.tar.gz").is_err()
        );
        assert_eq!(fs::read(&installed).unwrap(), b"old");

        install_unix_binary(&archive, &installed, Format::LinuxTar, &name).unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"new");
        assert_ne!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(fs::read(&sibling).unwrap(), b"original readme");
    }
}
