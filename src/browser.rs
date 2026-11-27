use anyhow::{Context as _, Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub(crate) const VERSION: &str = "0.43.0";
pub(crate) const INSTALL_REQUIRED: &str =
    "Install browser support in Settings → Browser to use @browser.";

struct Artifact {
    name: &'static str,
    bytes: u64,
    sha256: &'static str,
}

fn artifact() -> Option<Artifact> {
    // Pinned upstream release assets. Chrome for Testing has no Linux arm64 build.
    let (name, bytes, sha256) = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        if cfg!(target_env = "musl") {
            (
                "agent-browser-linux-musl-x64",
                20_242_928,
                "10b3b1d5fa8f53f7bc44273bcefaa64299ab77071bc83e75d96de5ed66025c89",
            )
        } else {
            (
                "agent-browser-linux-x64",
                20_460_232,
                "8e69c9fb662514c9cf36e4911e94e913742ba066e55102486350e049dd847612",
            )
        }
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "agent-browser-darwin-arm64",
            17_755_040,
            "05338a8eeddf0b3f9ebf0c87a6c21aedc10685608c68618e1a8483c43eae9c3f",
        )
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        (
            "agent-browser-darwin-x64",
            19_491_696,
            "49a5ab27297213c1bc0005701a6a4542399abfc9d2cb7520ab519ebb94b8b13d",
        )
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        (
            "agent-browser-win32-x64.exe",
            20_043_776,
            "ebcef5ebea93c7ccf0fe3f8c18eee908ae9174dfef8e52a3dda6f24a59f630dc",
        )
    } else {
        return None;
    };
    Some(Artifact {
        name,
        bytes,
        sha256,
    })
}

pub(crate) fn supported() -> bool {
    artifact().is_some()
}

fn helper_path() -> Result<PathBuf> {
    Ok(std::path::absolute(crate::paths::data_dir())?
        .join("tools")
        .join(format!("agent-browser-{VERSION}"))
        .join(if cfg!(windows) {
            "agent-browser.exe"
        } else {
            "agent-browser"
        }))
}

pub(crate) fn installed() -> Result<Option<PathBuf>> {
    let Some(artifact) = artifact() else {
        return Ok(None);
    };
    let path = helper_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == artifact.bytes => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if metadata.permissions().mode() & 0o100 == 0 {
                    return Ok(None);
                }
            }
            Ok(Some(path))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Could not check the browser installation"),
    }
}

pub(crate) fn install() -> Result<PathBuf> {
    let artifact = artifact().context("Browser installation is unavailable on this platform")?;
    let path = helper_path()?;
    let dir = path
        .parent()
        .expect("browser helper has an installation directory");
    fs::create_dir_all(dir).context("Could not create the browser installation directory")?;
    let source = reqwest::blocking::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?
        .get(format!(
            "https://github.com/vercel-labs/agent-browser/releases/download/v{VERSION}/{}",
            artifact.name
        ))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("Could not download agent-browser")?;
    let mut staged = tempfile::Builder::new()
        .prefix(".agent-browser-")
        .suffix(std::env::consts::EXE_SUFFIX)
        .tempfile_in(dir)?;
    copy_verified(source, &mut staged, &artifact).context("Could not verify agent-browser")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let staged = staged.into_temp_path();
    install_chrome(&staged, dir)?;
    fs::write(dir.join("LICENSE"), include_bytes!("../LICENSE-APACHE"))?;
    fs::write(
        dir.join("NOTICE"),
        "agent-browser Copyright 2025 Vercel Inc. Licensed under the Apache License, Version 2.0.\n",
    )?;
    staged
        .persist(&path)
        .context("Could not install agent-browser")?;
    Ok(path)
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not remove agent-browser"),
    }
}

fn copy_verified(source: impl Read, mut destination: impl Write, artifact: &Artifact) -> Result<()> {
    let mut source = source.take(artifact.bytes + 1);
    let mut buffer = [0; 32 * 1024];
    let mut hash = Sha256::new();
    let mut total = 0;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= artifact.bytes, "Unexpected download size");
        let chunk = &buffer[..count];
        hash.update(chunk);
        destination.write_all(chunk)?;
    }
    ensure!(total == artifact.bytes, "Incomplete download");
    ensure!(
        format!("{:x}", hash.finalize()) == artifact.sha256,
        "SHA-256 checksum mismatch"
    );
    Ok(())
}

fn install_chrome(helper: &Path, dir: &Path) -> Result<()> {
    let mut command = Command::new(helper);
    command
        .arg("install")
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .context("Could not start the Chrome installer")?;
    let mut stderr = child
        .stderr
        .take()
        .expect("Chrome installer stderr is piped");
    let mut output = Vec::new();
    let read = stderr
        .by_ref()
        .take(16 * 1024)
        .read_to_end(&mut output)
        .and_then(|_| io::copy(&mut stderr, &mut io::sink()));
    drop(stderr);
    let status = child.wait()?;
    if status.success() {
        return Ok(());
    }
    read?;
    let output = String::from_utf8_lossy(&output);
    let detail = output
        .rsplit(['\r', '\n'])
        .map(str::trim)
        .find(|line| !line.is_empty());
    Err(match detail {
        Some(detail) => anyhow!("Chrome for Testing installation failed: {detail}"),
        None => anyhow!("Chrome for Testing installation failed ({status})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_download_rejects_corruption_and_wrong_lengths() {
        let artifact = Artifact {
            name: "fixture",
            bytes: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        };
        let mut destination = Vec::new();
        copy_verified(b"hello".as_slice(), &mut destination, &artifact).unwrap();
        assert_eq!(destination, b"hello");
        for invalid in [b"hella".as_slice(), b"hell", b"hello!"] {
            assert!(copy_verified(invalid, io::sink(), &artifact).is_err());
        }
    }
}
