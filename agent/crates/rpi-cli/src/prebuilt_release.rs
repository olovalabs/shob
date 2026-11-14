//! CLI adaptation of the prebuilt package protocol. Executables use `binary`
//! instead of `library`; the CLI's existing versioned Release tag remains vX.Y.Z.
use crate::prebuilt_extensions::{
    body_limited, canonical_repository, check_hash, extract_library, host_target,
    validate_executable_target, validate_release_asset_url,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const REPOSITORY: &str = "https://github.com/bigfish1913/pi-rust";
const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    name: String,
    version: String,
    repository: String,
    tag: String,
    source_commit: String,
    targets: BTreeMap<String, Distribution>,
}

#[derive(Debug, Deserialize)]
struct Distribution {
    url: String,
    format: String,
    size: u64,
    sha256: String,
    binary: Binary,
}

#[derive(Debug, Deserialize)]
struct Binary {
    path: String,
    sha256: String,
}

fn validate<'a>(
    manifest: &'a Manifest,
    version: &str,
    target: &str,
) -> Result<&'a Distribution, String> {
    semver::Version::parse(version).map_err(|_| "invalid CLI release version")?;
    if manifest.schema_version != 1
        || manifest.name != "rpi-cli"
        || manifest.version != version
        || canonical_repository(&manifest.repository)? != REPOSITORY
        || manifest.tag != format!("v{version}")
    {
        return Err(
            "CLI manifest schema, package, version, repository or tag does not match".into(),
        );
    }
    if manifest.source_commit.len() != 40
        || !manifest
            .source_commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("CLI manifest must identify its full source commit".into());
    }
    let distribution = manifest.targets.get(target).ok_or_else(|| {
        format!("no prebuilt CLI for exact target {target}; select --build explicitly")
    })?;
    if distribution.size == 0
        || distribution.size > MAX_ARCHIVE
        || !matches!(distribution.format.as_str(), "zip" | "tar.gz")
    {
        return Err("invalid CLI archive format or size".into());
    }
    if distribution.binary.path
        != if target.contains("windows") {
            "rpi.exe"
        } else {
            "rpi"
        }
    {
        return Err("CLI manifest must declare the flat rpi executable filename".into());
    }
    for sha in [&distribution.sha256, &distribution.binary.sha256] {
        if sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid CLI manifest SHA-256".into());
        }
    }
    validate_release_asset_url(&distribution.url, REPOSITORY, &manifest.tag)?;
    Ok(distribution)
}

pub(crate) async fn download_to(
    version: &str,
    staging: &Path,
    cdn_base: Option<&reqwest::Url>,
) -> Result<(), String> {
    semver::Version::parse(version).map_err(|_| "invalid CLI release version")?;
    let target = host_target()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{REPOSITORY}/releases/download/v{version}/rpi-release.json");
    println!("downloading prebuilt rpi {version} for {target}");
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("CLI manifest request failed: {e}"))?;
    let manifest = read_manifest(response, version).await?;
    let path = download_manifest_to(&client, &manifest, version, target, staging, cdn_base).await?;
    crate::updates::validate_downloaded_rpi(&path, staging, version)
}

async fn read_manifest(response: reqwest::Response, version: &str) -> Result<Manifest, String> {
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("prebuilt CLI release {version} is not ready (rpi-release.json is missing); retry later or select --build explicitly"));
    }
    if !response.status().is_success() {
        return Err(format!(
            "CLI manifest request returned {}",
            response.status()
        ));
    }
    serde_json::from_slice(&body_limited(response, MAX_MANIFEST).await?)
        .map_err(|e| format!("invalid rpi-release.json: {e}"))
}

async fn download_manifest_to(
    client: &reqwest::Client,
    manifest: &Manifest,
    version: &str,
    target: &str,
    staging: &Path,
    cdn_base: Option<&reqwest::Url>,
) -> Result<PathBuf, String> {
    let distribution = validate(manifest, version, target)?;
    let cdn_url = cdn_base
        .map(|base| crate::cdn::archive_url(base, REPOSITORY, &manifest.tag, &distribution.url))
        .transpose()?
        .flatten();
    let archive = crate::cdn::download_with_fallback(
        client,
        cdn_url.as_ref(),
        &distribution.url,
        distribution.size,
        &distribution.sha256,
        "CLI archive",
    )
    .await?;
    stage_archive(manifest, version, target, &archive, staging)
}

fn stage_archive(
    manifest: &Manifest,
    version: &str,
    target: &str,
    archive: &[u8],
    staging: &Path,
) -> Result<PathBuf, String> {
    let distribution = validate(manifest, version, target)?;
    if archive.len() as u64 != distribution.size {
        return Err("CLI archive length does not match manifest".into());
    }
    check_hash(archive, &distribution.sha256)?;
    let binary = extract_library(archive, &distribution.format, &distribution.binary.path)?;
    check_hash(&binary, &distribution.binary.sha256)?;
    validate_executable_target(&binary, target)?;
    let bin = staging.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let path = bin.join(&distribution.binary.path);
    // Staging is a fresh private directory owned by the existing updater.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(&binary)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prebuilt_extensions::hash;
    use std::io::Write;

    #[tokio::test]
    async fn missing_and_malformed_manifests_report_errors_without_staging() {
        use std::io::Read;
        for (status, body, expected) in [
            ("404 Not Found", "missing", "rpi-release.json is missing"),
            ("403 Forbidden", "denied", "403 Forbidden"),
            ("200 OK", "not json", "invalid rpi-release.json"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let error = read_manifest(client.get(url).send().await.unwrap(), "1.2.3")
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
            server.join().unwrap();
        }
    }

    /// Opt-in network smoke uses a local manifest assembled from actual Release
    /// assets. It downloads/stages/executes --version, never replaces the host.
    #[tokio::test]
    #[ignore = "requires RPI_RELEASE_SMOKE_MANIFEST and GitHub access"]
    async fn live_github_release_download_and_exact_version_validation() {
        let path = std::env::var("RPI_RELEASE_SMOKE_MANIFEST").unwrap();
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .user_agent("rpi-release-smoke")
            .build()
            .unwrap();
        let executable = download_manifest_to(
            &client,
            &manifest,
            &manifest.version,
            host_target().unwrap(),
            staging.path(),
            None,
        )
        .await
        .unwrap();
        crate::updates::validate_downloaded_rpi(&executable, staging.path(), &manifest.version)
            .unwrap();
        assert!(
            crate::updates::validate_downloaded_rpi(&executable, staging.path(), "999.0.0")
                .is_err()
        );
    }

    fn fixture() -> (serde_json::Value, Vec<u8>) {
        let mut executable = vec![0; 128];
        executable[..2].copy_from_slice(b"MZ");
        executable[0x3c] = 64;
        executable[64..68].copy_from_slice(b"PE\0\0");
        executable[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        executable[86] = 2;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("rpi.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&executable).unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let manifest = serde_json::json!({"schema_version":1,"name":"rpi-cli","version":"1.2.3","repository":REPOSITORY,"tag":"v1.2.3","source_commit":"a".repeat(40),"targets":{"x86_64-pc-windows-msvc":{"url":format!("{REPOSITORY}/releases/download/v1.2.3/rpi.zip"),"format":"zip","size":archive.len(),"sha256":hash(&archive),"binary":{"path":"rpi.exe","sha256":hash(&executable)}}}});
        (manifest, archive)
    }

    #[test]
    fn executable_distribution_stages_only_after_both_checksums_and_identity_pass() {
        let (value, archive) = fixture();
        let target = "x86_64-pc-windows-msvc";
        let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = stage_archive(&manifest, "1.2.3", target, &archive, directory.path()).unwrap();
        assert_eq!(std::fs::read(path).unwrap()[86], 2);
        for (pointer, replacement) in [
            ("/schema_version", serde_json::json!(2)),
            ("/version", serde_json::json!("9.9.9")),
            (
                "/repository",
                serde_json::json!("https://github.com/other/repo"),
            ),
            ("/tag", serde_json::json!("rpi-cli-v1.2.3")),
            ("/source_commit", serde_json::json!("short")),
            (
                "/targets/x86_64-pc-windows-msvc/binary/path",
                serde_json::json!("../rpi.exe"),
            ),
            (
                "/targets/x86_64-pc-windows-msvc/sha256",
                serde_json::json!("0".repeat(64)),
            ),
            (
                "/targets/x86_64-pc-windows-msvc/binary/sha256",
                serde_json::json!("0".repeat(64)),
            ),
            (
                "/targets/x86_64-pc-windows-msvc/size",
                serde_json::json!(archive.len() + 1),
            ),
        ] {
            let mut bad = value.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            let manifest = serde_json::from_value(bad).unwrap();
            let directory = tempfile::tempdir().unwrap();
            assert!(
                stage_archive(&manifest, "1.2.3", target, &archive, directory.path()).is_err(),
                "{pointer}"
            );
            assert!(!directory.path().join("bin").exists());
        }
        assert!(validate(&manifest, "1.2.3", "aarch64-unknown-linux-gnu").is_err());
    }

    #[test]
    fn libraries_and_wrong_architectures_cannot_be_used_as_cli_executables() {
        let mut dll = vec![0; 128];
        dll[..2].copy_from_slice(b"MZ");
        dll[0x3c] = 64;
        dll[64..68].copy_from_slice(b"PE\0\0");
        dll[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        dll[86..88].copy_from_slice(&0x2002u16.to_le_bytes());
        assert!(validate_executable_target(&dll, "x86_64-pc-windows-msvc").is_err());
        dll[86..88].copy_from_slice(&2u16.to_le_bytes());
        assert!(validate_executable_target(&dll, "x86_64-pc-windows-msvc").is_ok());
        dll[68..70].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert!(validate_executable_target(&dll, "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn unix_executable_architectures_and_file_types_are_exact() {
        for (target, machine) in [
            ("x86_64-unknown-linux-gnu", 62u16),
            ("aarch64-unknown-linux-gnu", 183),
        ] {
            let mut elf = vec![0; 64];
            elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
            elf[16..18].copy_from_slice(&2u16.to_le_bytes());
            elf[18..20].copy_from_slice(&machine.to_le_bytes());
            assert!(validate_executable_target(&elf, target).is_ok());
            elf[16..18].copy_from_slice(&3u16.to_le_bytes());
            assert!(validate_executable_target(&elf, target).is_ok()); // position-independent executable
            elf[18..20].copy_from_slice(&0u16.to_le_bytes());
            assert!(validate_executable_target(&elf, target).is_err());
        }
        for (target, cpu) in [
            ("x86_64-apple-darwin", 0x01000007u32),
            ("aarch64-apple-darwin", 0x0100000c),
        ] {
            let mut macho = vec![0; 64];
            macho[..4].copy_from_slice(&0xfeedfacfu32.to_le_bytes());
            macho[4..8].copy_from_slice(&cpu.to_le_bytes());
            macho[12..16].copy_from_slice(&2u32.to_le_bytes());
            assert!(validate_executable_target(&macho, target).is_ok());
            macho[12..16].copy_from_slice(&6u32.to_le_bytes());
            assert!(validate_executable_target(&macho, target).is_err());
        }
    }
}
