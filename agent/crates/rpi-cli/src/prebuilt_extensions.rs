//! Versioned prebuilt extension distribution (rpi-extension.json v1).
//! Repository discovery uses crates.io metadata. Release tags and library
//! paths are protocol fields, never guessed from asset names or repository latest.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CRATES_API: &str = "https://crates.io/api/v1/crates";
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_LIBRARY: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
/// Optional archive index for sidecar files beside the library. Archives
/// without it are the legacy single-library layout.
const RUNTIME_ASSETS_FILE: &str = "runtime-assets.json";
/// Largest accepted `runtime-assets.json`, in bytes.
const MAX_ASSET_INDEX: u64 = 256 * 1024;
/// Longest accepted sidecar path, in bytes.
const MAX_ASSET_PATH: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrebuiltRecord {
    pub repository: String,
    pub target: String,
    pub manifest_sha256: String,
    pub source_commit: String,
    pub sdk_version: String,
    pub plugin_abi: u32,
    pub archive_sha256: String,
    pub library_sha256: String,
    /// Explicit repository installs discover versions from package-specific tags.
    #[serde(default)]
    pub custom_repository: bool,
    /// Sidecar files installed beside the library, as verified against the
    /// archive index. Empty for single-library archives, so their records are unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<PrebuiltAsset>,
}

/// One verified sidecar file, relative to the extension store directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrebuiltAsset {
    pub path: String,
    pub sha256: String,
}

/// The sidecar index inside a package archive: `runtime-assets.json`.
#[derive(Debug, Deserialize)]
struct RuntimeAssetIndex {
    schema_version: u32,
    assets: Vec<RuntimeAsset>,
}

#[derive(Debug, Deserialize)]
struct RuntimeAsset {
    path: String,
    size: u64,
    sha256: String,
}

/// Size caps for one archive: each file, and every expanded byte together.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveLimits {
    pub per_file: u64,
    pub total: u64,
}

impl ArchiveLimits {
    pub(crate) const PREBUILT: Self = Self {
        per_file: MAX_LIBRARY,
        total: MAX_ARCHIVE,
    };
}

/// A sidecar whose bytes match the archive index.
pub(crate) struct VerifiedAsset {
    pub path: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Verified archive contents. `assets` follow the order of the archive index.
pub(crate) struct PrebuiltContents {
    pub library: Vec<u8>,
    pub assets: Vec<VerifiedAsset>,
}

#[derive(Debug, Deserialize)]
struct RegistryPackage {
    #[serde(rename = "crate")]
    metadata: RegistryMetadata,
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Deserialize)]
struct RegistryMetadata {
    id: String,
    repository: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersion {
    num: String,
    yanked: bool,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    name: String,
    version: String,
    repository: String,
    tag: String,
    source_commit: String,
    sdk: Sdk,
    targets: BTreeMap<String, Distribution>,
}

#[derive(Debug, Deserialize)]
struct Sdk {
    version: String,
    plugin_abi: u32,
}

#[derive(Debug, Deserialize)]
struct Distribution {
    url: String,
    format: String,
    size: u64,
    sha256: String,
    library: Library,
}

#[derive(Debug, Deserialize)]
struct Library {
    path: String,
    sha256: String,
}

struct ResolvedPackage {
    name: String,
    version: String,
    repository: String,
    custom_repository: bool,
}

#[derive(Deserialize)]
struct GitReference {
    #[serde(rename = "ref")]
    reference: String,
}

fn stable_tag_version(refs: &[GitReference], name: &str) -> Result<String, String> {
    let prefix = format!("refs/tags/{name}-v");
    refs.iter().filter_map(|r| r.reference.strip_prefix(&prefix))
        .filter_map(|v| semver::Version::parse(v).ok()).filter(|v| v.pre.is_empty())
        .max().map(|v| v.to_string()).ok_or_else(|| format!("no stable {name}-v<version> tag exists in this repository; specify --version explicitly"))
}

async fn repository_version(
    client: &reqwest::Client,
    api: &str,
    name: &str,
    repository: &str,
) -> Result<String, String> {
    let repository = canonical_repository(repository)?;
    let repo = repository
        .strip_prefix("https://github.com/")
        .ok_or("invalid GitHub repository")?;
    let response = client
        .get(format!(
            "{api}/repos/{repo}/git/matching-refs/tags/{name}-v"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GitHub tag lookup failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub tag lookup returned {}", response.status()));
    }
    let refs: Vec<GitReference> =
        serde_json::from_slice(&body_limited(response, MAX_METADATA).await?)
            .map_err(|e| format!("invalid GitHub tag metadata: {e}"))?;
    stable_tag_version(&refs, name)
}

/// Supported triples are deliberately exact: GNU/musl and MSVC/GNU are
/// different native-library distributions.
pub fn host_target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") if cfg!(target_env = "msvc") => Ok("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "x86_64") if cfg!(target_env = "musl") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(
            "no supported prebuilt target for this host; use --build to compile explicitly".into(),
        ),
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Accept an explicit owner/repo override or repository metadata URLs,
/// including Cargo's common .git / git+https forms and GitHub tree subpaths.
pub(crate) fn canonical_repository(value: &str) -> Result<String, String> {
    let value = value.trim().strip_prefix("git+").unwrap_or(value.trim());
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://github.com/{value}")
    };
    let url = reqwest::Url::parse(&value).map_err(|_| "invalid GitHub repository".to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("prebuilt repository must be an HTTPS GitHub repository; use --build for other repositories".into());
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or("invalid GitHub repository path")?
        .collect();
    if segments.len() < 2 || !valid_segment(segments[0]) {
        return Err("repository must identify a GitHub owner and repository".into());
    }
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if !valid_segment(repo) {
        return Err("invalid GitHub repository name".into());
    }
    if segments.len() > 2
        && !segments[2..].iter().all(|s| s.is_empty())
        && !matches!(segments[2], "tree" | "blob")
    {
        return Err("unsupported GitHub repository URL path".into());
    }
    Ok(format!(
        "https://github.com/{}/{}",
        segments[0].to_ascii_lowercase(),
        repo.to_ascii_lowercase()
    ))
}

fn release_url(repository: &str, name: &str, version: &str, file: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(repository).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|_| "invalid release repository")?
        .push("releases")
        .push("download")
        .push(&format!("{name}-v{version}"))
        .push(file);
    Ok(url.into())
}

fn select_version(versions: &[RegistryVersion], requested: Option<&str>) -> Result<String, String> {
    if let Some(requested) = requested {
        let version = semver::Version::parse(requested)
            .map_err(|_| "--version requires an exact semantic version".to_string())?;
        return versions
            .iter()
            .find(|v| !v.yanked && v.num == version.to_string())
            .map(|v| v.num.clone())
            .ok_or_else(|| format!("version {requested} is absent or yanked on crates.io"));
    }
    versions
        .iter()
        .filter(|v| !v.yanked)
        .filter_map(|v| semver::Version::parse(&v.num).ok())
        .filter(|v| v.pre.is_empty())
        .max()
        .map(|v| v.to_string())
        .ok_or_else(|| "no non-yanked stable version is available".into())
}

pub(crate) async fn body_limited(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > limit) {
        return Err("download exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("download failed: {e}"))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err("download exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn resolve_package(
    client: &reqwest::Client,
    api: &str,
    name: &str,
    version: Option<&str>,
    repository: Option<&str>,
) -> Result<ResolvedPackage, String> {
    // An explicit repository + version also supports unpublished third-party extensions.
    if let Some(repository) = repository {
        let repository = canonical_repository(repository)?;
        let version = match version {
            Some(version) => semver::Version::parse(version)
                .map_err(|_| "--version requires an exact semantic version".to_string())?
                .to_string(),
            None => repository_version(client, "https://api.github.com", name, &repository).await?,
        };
        return Ok(ResolvedPackage {
            name: name.into(),
            version,
            repository,
            custom_repository: true,
        });
    }
    let response = client
        .get(format!("{api}/{name}"))
        .send()
        .await
        .map_err(|e| format!("crates.io lookup failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "crates.io lookup for {name} returned {}",
            response.status()
        ));
    }
    let data: RegistryPackage =
        serde_json::from_slice(&body_limited(response, MAX_METADATA).await?)
            .map_err(|e| format!("invalid crates.io metadata: {e}"))?;
    if !data
        .metadata
        .id
        .replace('-', "_")
        .eq_ignore_ascii_case(&name.replace('-', "_"))
    {
        return Err("crates.io metadata identifies a different package".into());
    }
    let version = select_version(&data.versions, version)?;
    let repository = repository
        .or(data.metadata.repository.as_deref())
        .ok_or("crate has no repository metadata; use --github owner/repo or --build")?;
    Ok(ResolvedPackage {
        name: data.metadata.id,
        version,
        repository: canonical_repository(repository)?,
        custom_repository: false,
    })
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn check_hash(bytes: &[u8], expected: &str) -> Result<(), String> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("manifest SHA-256 must be 64 lowercase hexadecimal characters".into());
    }
    if hash(bytes) != expected {
        return Err("SHA-256 mismatch".into());
    }
    Ok(())
}

fn validate_manifest<'a>(
    manifest: &'a Manifest,
    package: &ResolvedPackage,
    target: &str,
) -> Result<&'a Distribution, String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "unsupported prebuilt manifest schema {}",
            manifest.schema_version
        ));
    }
    if manifest.name != package.name
        || manifest.version != package.version
        || manifest.tag != format!("{}-v{}", package.name, package.version)
        || canonical_repository(&manifest.repository)? != package.repository
    {
        return Err("prebuilt manifest package, version, repository or tag does not match the requested package".into());
    }
    if manifest.source_commit.len() != 40
        || !manifest
            .source_commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("manifest source_commit must be a full Git commit SHA".into());
    }
    semver::Version::parse(&manifest.sdk.version).map_err(|_| "invalid SDK version in manifest")?;
    if manifest.sdk.plugin_abi != rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED {
        return Err(format!(
            "extension SDK ABI {} is incompatible with host ABI {}",
            manifest.sdk.plugin_abi,
            rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED
        ));
    }
    let distribution = manifest.targets.get(target).ok_or_else(|| {
        format!(
            "no prebuilt extension for exact target {target}; use --build to compile explicitly"
        )
    })?;
    if distribution.size == 0
        || distribution.size > MAX_ARCHIVE
        || !matches!(distribution.format.as_str(), "zip" | "tar.gz")
    {
        return Err("unsupported archive format or size in manifest".into());
    }
    let name = &distribution.library.path;
    let extension = if target.contains("windows") {
        ".dll"
    } else if target.contains("apple") {
        ".dylib"
    } else {
        ".so"
    };
    if name.is_empty()
        || !name.ends_with(extension)
        || name.contains(['/', '\\', ':'])
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        || matches!(name.as_str(), "." | "..")
    {
        return Err(
            "manifest library.path must be a flat library filename for the selected target".into(),
        );
    }
    for expected in [&distribution.sha256, &distribution.library.sha256] {
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid SHA-256 in manifest".into());
        }
    }
    validate_release_asset_url(&distribution.url, &package.repository, &manifest.tag)?;
    Ok(distribution)
}

pub(crate) fn validate_release_asset_url(
    value: &str,
    repository: &str,
    tag: &str,
) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid archive URL")?;
    let segments: Vec<String> = url
        .path_segments()
        .ok_or("archive URL has no path")?
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| "invalid encoded archive URL")
        })
        .collect::<Result<_, _>>()?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || segments.len() != 6
        || segments[2] != "releases"
        || segments[3] != "download"
        || segments[4] != tag
        || canonical_repository(&format!(
            "https://github.com/{}/{}",
            segments[0], segments[1]
        ))? != repository
        || segments[5].is_empty()
        || matches!(segments[5].as_str(), "." | "..")
        || !segments[5]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
    {
        return Err(
            "archive URL must belong to the same GitHub repository and package Release".into(),
        );
    }
    Ok(())
}

async fn fetch_manifest(
    client: &reqwest::Client,
    package: &ResolvedPackage,
) -> Result<(Manifest, Vec<u8>), String> {
    let url = release_url(
        &package.repository,
        &package.name,
        &package.version,
        "rpi-extension.json",
    )?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("prebuilt manifest request failed: {e}"))?;
    read_manifest_response(response, package).await
}

async fn read_manifest_response(
    response: reqwest::Response,
    package: &ResolvedPackage,
) -> Result<(Manifest, Vec<u8>), String> {
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("prebuilt release for {} {} is not ready (rpi-extension.json is missing); retry later or use --build explicitly", package.name, package.version));
    }
    if !response.status().is_success() {
        return Err(format!(
            "prebuilt manifest request returned {}",
            response.status()
        ));
    }
    let bytes = body_limited(response, MAX_MANIFEST).await?;
    let manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid rpi-extension.json: {e}"))?;
    Ok((manifest, bytes))
}

/// Only advertise an update when its exact platform manifest is available and compatible.
pub async fn latest_compatible_version(
    client: &reqwest::Client,
    package: &crate::install::InstalledNativePackage,
) -> Result<Option<String>, String> {
    let Some(record) = &package.prebuilt else {
        return Ok(None);
    };
    let repository = record
        .custom_repository
        .then_some(record.repository.as_str());
    let resolved = resolve_package(client, CRATES_API, &package.name, None, repository).await?;
    if resolved.repository != record.repository {
        return Err(
            "extension repository changed; reinstall explicitly to select a new publisher".into(),
        );
    }
    if !crate::updates::is_newer(&package.version, &resolved.version) {
        return Ok(Some(resolved.version));
    }
    let (manifest, _) = fetch_manifest(client, &resolved).await?;
    validate_manifest(&manifest, &resolved, host_target()?)?;
    Ok(Some(resolved.version))
}

pub async fn install(
    name: &str,
    version: Option<&str>,
    repository: Option<&str>,
    force: bool,
    cdn_base: Option<&reqwest::Url>,
) -> Result<(), String> {
    let records = crate::install::installed_native_packages_strict()?;
    let existing = records.iter().find(|p| p.name == name);
    let target = host_target()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;
    let repository = repository.or_else(|| {
        existing
            .and_then(|p| p.prebuilt.as_ref())
            .filter(|record| record.custom_repository)
            .map(|r| r.repository.as_str())
    });
    let package = resolve_package(&client, CRATES_API, name, version, repository).await?;
    if let Some(record) = existing.and_then(|p| p.prebuilt.as_ref()) {
        if repository.is_none() && record.repository != package.repository {
            return Err(
                "extension repository changed; select the new repository explicitly with --github"
                    .into(),
            );
        }
    }
    if !crate::install::should_install_version(
        name,
        existing.map(|p| p.version.as_str()),
        &package.version,
        force,
    )? {
        crate::install::print_already_installed(name, &package.version);
        return Ok(());
    }
    println!(
        "downloading prebuilt {} {} for {target}",
        package.name, package.version
    );
    let (manifest, manifest_bytes) = fetch_manifest(&client, &package).await?;
    let distribution = validate_manifest(&manifest, &package, target)?;
    let cdn_url = cdn_base
        .map(|base| {
            crate::cdn::archive_url(base, &package.repository, &manifest.tag, &distribution.url)
        })
        .transpose()?
        .flatten();
    let archive = crate::cdn::download_with_fallback(
        &client,
        cdn_url.as_ref(),
        &distribution.url,
        distribution.size,
        &distribution.sha256,
        "archive",
    )
    .await?;
    let agent_dir = crate::config::agent_dir().map_err(|e| e.to_string())?;
    let target = target.to_string();
    let installed = tokio::task::spawn_blocking(move || {
        install_archive_at(
            &package,
            &manifest,
            &manifest_bytes,
            &archive,
            &target,
            &agent_dir,
            force,
        )
    })
    .await
    .map_err(|e| format!("prebuilt installation failed: {e}"))??;
    for path in installed {
        println!("installed {}", path.display());
    }
    println!("rpi will validate the plugin entrypoint/SDK API size and load this extension on the next start.");
    Ok(())
}

fn install_archive_at(
    package: &ResolvedPackage,
    manifest: &Manifest,
    manifest_bytes: &[u8],
    archive: &[u8],
    target: &str,
    agent_dir: &Path,
    force: bool,
) -> Result<Vec<PathBuf>, String> {
    let distribution = validate_manifest(manifest, package, target)?;
    if archive.len() as u64 != distribution.size {
        return Err("archive byte length does not match manifest".into());
    }
    check_hash(archive, &distribution.sha256)?;
    // Everything is verified in memory before the extension store is touched.
    let contents = extract_contents(archive, &distribution.format, &distribution.library.path)?;
    check_hash(&contents.library, &distribution.library.sha256)?;
    validate_library_target(&contents.library, target)?;
    let provenance = PrebuiltRecord {
        repository: package.repository.clone(),
        target: target.into(),
        manifest_sha256: hash(manifest_bytes),
        source_commit: manifest.source_commit.clone(),
        sdk_version: manifest.sdk.version.clone(),
        plugin_abi: manifest.sdk.plugin_abi,
        archive_sha256: distribution.sha256.clone(),
        library_sha256: distribution.library.sha256.clone(),
        custom_repository: package.custom_repository,
        assets: contents
            .assets
            .iter()
            .map(|asset| PrebuiltAsset {
                path: asset.path.clone(),
                sha256: asset.sha256.clone(),
            })
            .collect(),
    };
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let artifact = temporary.path().join(&distribution.library.path);
    std::fs::write(&artifact, &contents.library).map_err(|e| e.to_string())?;
    let assets: Vec<crate::install::NativeAssetFile> = contents
        .assets
        .into_iter()
        .map(|asset| crate::install::NativeAssetFile {
            path: asset.path,
            bytes: asset.bytes,
        })
        .collect();
    let record = crate::install::InstalledNativePackage {
        name: package.name.clone(),
        version: package.version.clone(),
        // Older hosts regard this as a non-registry source and will not rebuild it.
        source: Some(format!("prebuilt:{}", package.repository)),
        prebuilt: Some(provenance),
        artifacts: Vec::new(),
    };
    crate::install::install_native_package_with_assets_at(
        &agent_dir.join("extensions"),
        &agent_dir.join("native-packages.json"),
        record,
        &[artifact],
        &assets,
        force,
    )
}

fn safe_entry_name(name: &str) -> Result<PathBuf, String> {
    if name.contains(['\\', ':'])
        || Path::new(name).components().any(|p| {
            matches!(
                p,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("unsafe archive entry path".into());
    }
    Ok(Path::new(name)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect())
}

fn read_library(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_LIBRARY + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_LIBRARY {
        return Err("extracted library exceeds size limit".into());
    }
    Ok(bytes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
}

/// Visit every entry of a prebuilt archive in archive order. Unsafe names, links,
/// more than `MAX_ENTRIES` entries and more than `total` expanded bytes are rejected
/// here, for every archive. Visitors decide what to read.
fn walk_archive(
    archive: &[u8],
    format: &str,
    total: u64,
    mut visit: impl FnMut(&Path, EntryKind, &mut dyn Read) -> Result<(), String>,
) -> Result<(), String> {
    let mut expanded = 0u64;
    match format {
        "zip" => {
            let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|e| e.to_string())?;
            if zip.len() > MAX_ENTRIES {
                return Err("too many archive entries".into());
            }
            for i in 0..zip.len() {
                let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
                let path = safe_entry_name(file.name())?;
                if file
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    return Err("archive symlinks are not allowed".into());
                }
                expanded = expanded
                    .checked_add(file.size())
                    .ok_or("archive size overflow")?;
                if expanded > total {
                    return Err("expanded archive exceeds size limit".into());
                }
                let kind = if file.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                visit(&path, kind, &mut file)?;
            }
        }
        "tar.gz" => {
            let decoder = flate2::read::GzDecoder::new(Cursor::new(archive)).take(total + 1);
            let mut tar = tar::Archive::new(decoder);
            for (i, entry) in tar.entries().map_err(|e| e.to_string())?.enumerate() {
                if i >= MAX_ENTRIES {
                    return Err("too many archive entries".into());
                }
                let mut file = entry.map_err(|e| e.to_string())?;
                let name = file
                    .path()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                let path = safe_entry_name(&name)?;
                let kind = file.header().entry_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err("archive links and special files are not allowed".into());
                }
                expanded = expanded
                    .checked_add(file.size())
                    .ok_or("archive size overflow")?;
                if expanded > total {
                    return Err("expanded archive exceeds size limit".into());
                }
                let kind = if kind.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                visit(&path, kind, &mut file)?;
            }
        }
        _ => return Err("unsupported prebuilt archive format".into()),
    }
    Ok(())
}

/// Read only the declared library. Legacy callers, such as the CLI's own release
/// archives, never carry sidecars, so every other entry is ignored.
pub(crate) fn extract_library(
    archive: &[u8],
    format: &str,
    library: &str,
) -> Result<Vec<u8>, String> {
    let mut found = None;
    walk_archive(archive, format, MAX_ARCHIVE, |path, kind, reader| {
        if kind == EntryKind::File && path == Path::new(library) {
            if found.is_some() {
                return Err("duplicate declared library in archive".into());
            }
            found = Some(read_library(reader)?);
        }
        Ok(())
    })?;
    found.ok_or_else(|| format!("archive does not contain the exact declared library {library}"))
}

/// Extract a prebuilt package: its library and, when the archive has a
/// `runtime-assets.json` index, every declared sidecar, all verified.
pub(crate) fn extract_contents(
    archive: &[u8],
    format: &str,
    library: &str,
) -> Result<PrebuiltContents, String> {
    extract_contents_with_limits(archive, format, library, ArchiveLimits::PREBUILT)
}

/// Two passes over the archive. The first reads the library and the index, so
/// the index can be validated before any sidecar byte is read. The second
/// reads each declared sidecar and rejects anything the index does not name.
/// Without an index the archive is the legacy single-library layout.
fn extract_contents_with_limits(
    archive: &[u8],
    format: &str,
    library: &str,
    limits: ArchiveLimits,
) -> Result<PrebuiltContents, String> {
    let library_path = Path::new(library);
    let index_path = Path::new(RUNTIME_ASSETS_FILE);
    let mut produced = 0u64;
    let mut library_bytes = None;
    let mut index_bytes = None;
    walk_archive(archive, format, limits.total, |path, kind, reader| {
        if kind == EntryKind::Directory {
            return Ok(());
        }
        if path == library_path {
            if library_bytes.is_some() {
                return Err("duplicate declared library in archive".into());
            }
            library_bytes = Some(read_budgeted(
                reader,
                &mut produced,
                limits,
                limits.per_file,
                "extracted library exceeds size limit",
            )?);
        } else if path == index_path {
            if index_bytes.is_some() {
                return Err(format!("duplicate {RUNTIME_ASSETS_FILE} in archive"));
            }
            index_bytes = Some(read_budgeted(
                reader,
                &mut produced,
                limits,
                MAX_ASSET_INDEX,
                "runtime-assets.json exceeds size limit",
            )?);
        }
        Ok(())
    })?;
    let library_bytes = library_bytes
        .ok_or_else(|| format!("archive does not contain the exact declared library {library}"))?;
    let Some(index_bytes) = index_bytes else {
        return Ok(PrebuiltContents {
            library: library_bytes,
            assets: Vec::new(),
        });
    };
    let declared = declared_assets(&index_bytes, library, limits)?;
    let mut found: Vec<Option<Vec<u8>>> = vec![None; declared.len()];
    walk_archive(archive, format, limits.total, |path, kind, reader| {
        if kind == EntryKind::Directory {
            if has_node_modules(path) {
                return Err(format!("archive contains node_modules: {}", path.display()));
            }
            return Ok(());
        }
        // Already read and checked by the first pass.
        if path == library_path || path == index_path {
            return Ok(());
        }
        let Some(position) = declared
            .iter()
            .position(|asset| Path::new(&asset.path) == path)
        else {
            return Err(format!("undeclared file in archive: {}", path.display()));
        };
        let asset = &declared[position];
        if found[position].is_some() {
            return Err(format!(
                "duplicate declared asset in archive: {}",
                asset.path
            ));
        }
        let bytes = read_budgeted(
            reader,
            &mut produced,
            limits,
            limits.per_file,
            "extracted asset exceeds size limit",
        )?;
        if bytes.len() as u64 != asset.size {
            return Err(format!(
                "asset {} size does not match runtime-assets.json",
                asset.path
            ));
        }
        check_hash(&bytes, &asset.sha256).map_err(|e| format!("asset {}: {e}", asset.path))?;
        found[position] = Some(bytes);
        Ok(())
    })?;
    let mut assets = Vec::with_capacity(declared.len());
    for (asset, bytes) in declared.into_iter().zip(found) {
        let bytes = bytes
            .ok_or_else(|| format!("archive does not contain declared asset {}", asset.path))?;
        assets.push(VerifiedAsset {
            path: asset.path,
            sha256: asset.sha256,
            bytes,
        });
    }
    Ok(PrebuiltContents {
        library: library_bytes,
        assets,
    })
}

/// Parse and validate `runtime-assets.json`. Paths, sizes, digests and collisions
/// are all checked before any sidecar is read.
fn declared_assets(
    bytes: &[u8],
    library: &str,
    limits: ArchiveLimits,
) -> Result<Vec<RuntimeAsset>, String> {
    let index: RuntimeAssetIndex =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid {RUNTIME_ASSETS_FILE}: {e}"))?;
    if index.schema_version != 1 {
        return Err(format!(
            "unsupported runtime asset index schema {}",
            index.schema_version
        ));
    }
    // The library and the index are two of the archive's entries.
    if index.assets.len() + 2 > MAX_ENTRIES {
        return Err("too many runtime assets".into());
    }
    let reserved = [
        library.to_ascii_lowercase(),
        RUNTIME_ASSETS_FILE.to_ascii_lowercase(),
    ];
    let mut declared: Vec<RuntimeAsset> = Vec::with_capacity(index.assets.len());
    for asset in index.assets {
        validate_asset_path(&asset.path)?;
        if reserved
            .iter()
            .any(|name| asset_paths_conflict(name, &asset.path))
        {
            return Err(format!(
                "asset path `{}` collides with the library or manifest",
                asset.path
            ));
        }
        if declared
            .iter()
            .any(|other| asset_paths_conflict(&other.path, &asset.path))
        {
            return Err(format!(
                "duplicate or overlapping asset path `{}`",
                asset.path
            ));
        }
        if asset.size > limits.per_file {
            return Err(format!("runtime asset {} exceeds size limit", asset.path));
        }
        if !is_sha256_hex(&asset.sha256) {
            return Err(format!("invalid SHA-256 for runtime asset {}", asset.path));
        }
        declared.push(asset);
    }
    Ok(declared)
}

/// Read one archive entry under its per-file cap and the archive's remaining
/// expansion budget. Budgets count bytes actually produced, so a header that
/// understates its size cannot bypass them.
fn read_budgeted(
    reader: &mut dyn Read,
    produced: &mut u64,
    limits: ArchiveLimits,
    per_file: u64,
    file_error: &str,
) -> Result<Vec<u8>, String> {
    let remaining = limits.total.saturating_sub(*produced);
    let limit = per_file.min(remaining);
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(if limit == remaining {
            "expanded archive exceeds size limit".into()
        } else {
            file_error.into()
        });
    }
    *produced += bytes.len() as u64;
    Ok(bytes)
}

/// Sidecar paths are relative, forward-slash and plain ASCII. Components cannot be
/// empty, `.`, `..` or hidden, cannot be a Windows device name or end in a dot,
/// cannot be `node_modules`, and cannot carry a native library extension, because
/// the loader scans the extension store for libraries.
pub(crate) fn validate_asset_path(value: &str) -> Result<(), String> {
    let invalid = || format!("invalid asset path `{value}`");
    if value.is_empty() || value.len() > MAX_ASSET_PATH {
        return Err(invalid());
    }
    for component in value.split('/') {
        // Windows maps these stems to devices even with an extension, and drops trailing dots.
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with('.')
            || device
            || !component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid());
        }
        if component.eq_ignore_ascii_case("node_modules") {
            return Err(format!(
                "asset `{value}` vendors node_modules; dependencies must be installed at runtime"
            ));
        }
        if crate::install::is_dynamic_library(Path::new(component)) {
            return Err(format!(
                "asset `{value}` has a native library extension and cannot be installed as an asset"
            ));
        }
    }
    Ok(())
}

/// Whether two store paths name one file, compared case-insensitively, or nest one under the other.
pub(crate) fn asset_paths_conflict(left: &str, right: &str) -> bool {
    let (left, right) = (left.to_ascii_lowercase(), right.to_ascii_lowercase());
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn has_node_modules(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("node_modules"))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Verify the native object format/architecture without executing downloaded code.
fn validate_library_target(bytes: &[u8], target: &str) -> Result<(), String> {
    validate_native_target(bytes, target, true)
}

pub(crate) fn validate_executable_target(bytes: &[u8], target: &str) -> Result<(), String> {
    validate_native_target(bytes, target, false)
}

fn validate_native_target(bytes: &[u8], target: &str, library: bool) -> Result<(), String> {
    let u16_at = |n: usize| {
        bytes
            .get(n..n.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |n: usize| {
        bytes
            .get(n..n.checked_add(4)?)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let valid = match target {
        "x86_64-pc-windows-msvc" => {
            let offset = u32_at(0x3c).unwrap_or(u32::MAX) as usize;
            bytes.starts_with(b"MZ")
                && bytes.get(offset..offset.saturating_add(4)) == Some(b"PE\0\0".as_slice())
                && u16_at(offset.saturating_add(4)) == Some(0x8664)
                && u16_at(offset.saturating_add(22)).is_some_and(|flags| {
                    if library {
                        flags & 0x2000 != 0
                    } else {
                        flags & 0x2000 == 0 && flags & 2 != 0
                    }
                })
        }
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" | "aarch64-unknown-linux-gnu" => {
            bytes.starts_with(b"\x7fELF\x02\x01")
                && u16_at(16).is_some_and(|kind| kind == 3 || (!library && kind == 2))
                && u16_at(18)
                    == Some(if target.starts_with("aarch64") {
                        183
                    } else {
                        62
                    })
        }
        "x86_64-apple-darwin" | "aarch64-apple-darwin" => {
            u32_at(0) == Some(0xfeedfacf)
                && u32_at(12) == Some(if library { 6 } else { 2 })
                && u32_at(4)
                    == Some(if target.starts_with("aarch64") {
                        0x0100000c
                    } else {
                        0x01000007
                    })
        }
        _ => false,
    };
    if !valid {
        return Err(format!(
            "declared artifact is not a compatible native {} for {target}",
            if library {
                "shared library"
            } else {
                "executable"
            }
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TARGETS: [&str; 6] = [
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ];

    fn object(target: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        if target.contains("windows") {
            bytes[..2].copy_from_slice(b"MZ");
            bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
            bytes[64..68].copy_from_slice(b"PE\0\0");
            bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
            bytes[86..88].copy_from_slice(&0x2000u16.to_le_bytes());
        } else if target.contains("linux") {
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
            bytes[18..20].copy_from_slice(
                &(if target.starts_with("aarch64") {
                    183u16
                } else {
                    62u16
                })
                .to_le_bytes(),
            );
        } else {
            bytes[..4].copy_from_slice(&0xfeedfacfu32.to_le_bytes());
            bytes[4..8].copy_from_slice(
                &(if target.starts_with("aarch64") {
                    0x0100000cu32
                } else {
                    0x01000007u32
                })
                .to_le_bytes(),
            );
            bytes[12..16].copy_from_slice(&6u32.to_le_bytes());
        }
        bytes
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn fixture(target: &str) -> (ResolvedPackage, serde_json::Value, Vec<u8>) {
        fixture_with_entries(target, &[])
    }

    /// The fixture release for `target`, with `extra` files archived beside the library.
    fn fixture_with_entries(
        target: &str,
        extra: &[(&str, &[u8])],
    ) -> (ResolvedPackage, serde_json::Value, Vec<u8>) {
        let package = ResolvedPackage {
            name: "independent-plugin".into(),
            version: "1.2.3".into(),
            repository: "https://github.com/publisher/monorepo".into(),
            custom_repository: false,
        };
        // The library target deliberately differs from the crate name.
        let (format, library, file) = if target.contains("windows") {
            ("zip", "custom_runtime.dll", "package.zip")
        } else if target.contains("apple") {
            ("tar.gz", "libcustom_runtime.dylib", "package.tar.gz")
        } else {
            ("tar.gz", "libcustom_runtime.so", "package.tar.gz")
        };
        let bytes = object(target);
        let mut entries = vec![(library, bytes.as_slice())];
        entries.extend_from_slice(extra);
        let archive = if format == "zip" {
            zip(&entries)
        } else {
            tgz(&entries)
        };
        let manifest = serde_json::json!({
            "schema_version":1, "name":package.name, "version":package.version,
            "repository":package.repository, "tag":"independent-plugin-v1.2.3", "source_commit":"a".repeat(40),
            "sdk":{"version":"0.3.23", "plugin_abi":rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED},
            "targets":{target:{"url":release_url(&package.repository,&package.name,&package.version,file).unwrap(),
                "format":format,"size":archive.len(),"sha256":hash(&archive),"library":{"path":library,"sha256":hash(&bytes)}}}
        });
        (package, manifest, archive)
    }

    #[test]
    fn declared_runtime_assets_install_beside_the_library() {
        let target = TARGETS[0];
        let worker = b"export const ready = true;\n";
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assets": [{"path": "runtime/main.mjs", "size": worker.len(), "sha256": hash(worker)}]
        }))
        .unwrap();
        let (package, value, archive) = fixture_with_entries(
            target,
            &[
                ("runtime-assets.json", &index[..]),
                ("runtime/main.mjs", &worker[..]),
            ],
        );
        let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
        let agent = tempfile::tempdir().unwrap();
        let paths = install_archive_at(
            &package,
            &manifest,
            &serde_json::to_vec(&value).unwrap(),
            &archive,
            target,
            agent.path(),
            false,
        )
        .unwrap();
        let extensions = agent.path().join("extensions");
        assert_eq!(
            std::fs::read(extensions.join("runtime/main.mjs")).unwrap(),
            worker
        );
        assert!(paths.contains(&extensions.join("runtime/main.mjs")));
        assert!(!extensions.join("runtime-assets.json").exists());
    }

    #[test]
    fn runtime_asset_index_rejects_unsafe_or_inconsistent_declarations() {
        let library = object(TARGETS[0]);
        let worker: &[u8] = b"export const ready = true;\n";
        let entry = |path: &str, bytes: &[u8]| serde_json::json!({"path": path, "size": bytes.len(), "sha256": hash(bytes)});
        let index = |assets: Vec<serde_json::Value>| {
            serde_json::to_vec(&serde_json::json!({"schema_version": 1, "assets": assets})).unwrap()
        };
        let many: Vec<serde_json::Value> = (0..127)
            .map(|i| entry(&format!("assets/f{i}.mjs"), worker))
            .collect();
        let cases: Vec<(&str, Vec<u8>, Vec<(&str, Vec<u8>)>, &str)> = vec![
            (
                "absolute path",
                index(vec![entry("/etc/main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "parent traversal",
                index(vec![entry("../main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "dot component",
                index(vec![entry("./main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "backslash",
                index(vec![entry("runtime\\main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "drive prefix",
                index(vec![entry("C:main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "hidden component",
                index(vec![entry(".env", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "trailing slash",
                index(vec![entry("runtime/", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "overlong path",
                index(vec![entry(&format!("{}.mjs", "a".repeat(510)), worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "node_modules",
                index(vec![entry("runtime/node_modules/dep.mjs", worker)]),
                vec![],
                "vendors node_modules",
            ),
            (
                "native library",
                index(vec![entry("native/helper.so", worker)]),
                vec![],
                "native library extension",
            ),
            (
                "index name",
                index(vec![entry("runtime-assets.json", worker)]),
                vec![],
                "collides with the library or manifest",
            ),
            (
                "case-folded duplicate",
                index(vec![
                    entry("runtime/main.mjs", worker),
                    entry("Runtime/Main.mjs", worker),
                ]),
                vec![],
                "duplicate or overlapping",
            ),
            (
                "nested path",
                index(vec![
                    entry("runtime", worker),
                    entry("runtime/main.mjs", worker),
                ]),
                vec![],
                "duplicate or overlapping",
            ),
            (
                "malformed digest",
                index(vec![
                    serde_json::json!({"path": "main.mjs", "size": worker.len(), "sha256": "ABC"}),
                ]),
                vec![],
                "invalid SHA-256",
            ),
            (
                "windows device name",
                index(vec![entry("runtime/CON.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "windows bare device name",
                index(vec![entry("runtime/nul", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "windows trailing dot",
                index(vec![entry("runtime/main.mjs.", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "empty component",
                index(vec![entry("runtime//main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "unc prefix",
                index(vec![entry("\\\\server\\share\\main.mjs", worker)]),
                vec![],
                "invalid asset path",
            ),
            (
                "missing digest",
                index(vec![
                    serde_json::json!({"path": "main.mjs", "size": worker.len()}),
                ]),
                vec![("main.mjs", worker.to_vec())],
                "invalid runtime-assets.json",
            ),
            (
                "uppercase digest",
                index(vec![
                    serde_json::json!({"path": "main.mjs", "size": worker.len(), "sha256": hash(worker).to_uppercase()}),
                ]),
                vec![("main.mjs", worker.to_vec())],
                "invalid SHA-256",
            ),
            (
                "traversing archive entry",
                index(vec![]),
                vec![("../escape.mjs", worker.to_vec())],
                "unsafe archive entry path",
            ),
            (
                "absolute archive entry",
                index(vec![]),
                vec![("/escape.mjs", worker.to_vec())],
                "unsafe archive entry path",
            ),
            (
                "backslash archive entry",
                index(vec![entry("runtime/main.mjs", worker)]),
                vec![("runtime\\main.mjs", worker.to_vec())],
                "unsafe archive entry path",
            ),
            (
                "case-folded archive entry",
                index(vec![entry("runtime/main.mjs", worker)]),
                vec![("Runtime/Main.mjs", worker.to_vec())],
                "undeclared file in archive",
            ),
            (
                "unknown schema",
                serde_json::to_vec(&serde_json::json!({"schema_version": 2, "assets": []}))
                    .unwrap(),
                vec![],
                "unsupported runtime asset index schema",
            ),
            (
                "malformed index",
                b"{".to_vec(),
                vec![],
                "invalid runtime-assets.json",
            ),
            (
                "too many assets",
                index(many),
                vec![],
                "too many runtime assets",
            ),
            (
                "undeclared file",
                index(vec![]),
                vec![("runtime/main.mjs", worker.to_vec())],
                "undeclared file in archive",
            ),
            (
                "missing declared file",
                index(vec![entry("runtime/main.mjs", worker)]),
                vec![],
                "does not contain declared asset",
            ),
            (
                "size mismatch",
                index(vec![
                    serde_json::json!({"path": "main.mjs", "size": worker.len() + 1, "sha256": hash(worker)}),
                ]),
                vec![("main.mjs", worker.to_vec())],
                "size does not match",
            ),
            (
                "digest mismatch",
                index(vec![
                    serde_json::json!({"path": "main.mjs", "size": worker.len(), "sha256": hash(b"other")}),
                ]),
                vec![("main.mjs", worker.to_vec())],
                "SHA-256 mismatch",
            ),
        ];
        for (label, index, extra, expected) in cases {
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("custom_runtime.dll", library.as_slice()),
                ("runtime-assets.json", index.as_slice()),
            ];
            entries.extend(extra.iter().map(|(name, bytes)| (*name, bytes.as_slice())));
            let Err(error) = extract_contents(&zip(&entries), "zip", "custom_runtime.dll") else {
                panic!("{label} was accepted");
            };
            assert!(error.contains(expected), "{label}: {error}");
        }
    }

    #[test]
    fn runtime_asset_limits_bound_each_file_and_the_whole_archive() {
        let library = object(TARGETS[0]);
        let worker = vec![b'x'; 200];
        let index = |size: u64| {
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "assets": [{"path": "runtime/main.mjs", "size": size, "sha256": hash(&worker)}]
            }))
            .unwrap()
        };
        let declared = index(worker.len() as u64);
        let archive = |index: &[u8], worker: &[u8]| {
            zip(&[
                ("custom_runtime.dll", library.as_slice()),
                ("runtime-assets.json", index),
                ("runtime/main.mjs", worker),
            ])
        };
        let limits = |per_file, total| ArchiveLimits { per_file, total };

        // A sidecar larger than the per-file cap is refused by its declaration.
        let error = extract_contents_with_limits(
            &archive(&declared, &worker),
            "zip",
            "custom_runtime.dll",
            limits(128, u64::MAX),
        )
        .err()
        .unwrap();
        assert!(
            error.contains("runtime asset runtime/main.mjs exceeds size limit"),
            "{error}"
        );

        // A declaration that understates the bytes is stopped by the read cap.
        let understated = index(4);
        let error = extract_contents_with_limits(
            &archive(&understated, &worker),
            "zip",
            "custom_runtime.dll",
            limits(128, u64::MAX),
        )
        .err()
        .unwrap();
        assert!(
            error.contains("extracted asset exceeds size limit"),
            "{error}"
        );

        // The expanded total counts every entry, including the library and index.
        let exact = library.len() as u64 + declared.len() as u64 + worker.len() as u64;
        let contents = extract_contents_with_limits(
            &archive(&declared, &worker),
            "zip",
            "custom_runtime.dll",
            limits(u64::MAX, exact),
        )
        .unwrap();
        assert_eq!(contents.assets[0].bytes, worker);
        let error = extract_contents_with_limits(
            &archive(&declared, &worker),
            "zip",
            "custom_runtime.dll",
            limits(u64::MAX, exact - 1),
        )
        .err()
        .unwrap();
        assert!(
            error.contains("expanded archive exceeds size limit"),
            "{error}"
        );
    }

    #[test]
    fn archive_walk_bounds_expanded_size_for_every_archive() {
        // Undeclared entries are never read, so the header-declared total is the
        // only bound on a legacy archive's expansion.
        let archive = zip(&[("a.bin", &[0u8; 12][..]), ("b.bin", &[0u8; 12][..])]);
        let error = walk_archive(&archive, "zip", 20, |_, _, _| Ok(())).unwrap_err();
        assert!(
            error.contains("expanded archive exceeds size limit"),
            "{error}"
        );
        walk_archive(&archive, "zip", 24, |_, _, _| Ok(())).unwrap();

        // The tar.gz stream is capped as well, so its framing counts toward the total.
        let tar = tgz(&[("a.bin", &[0u8; 12][..]), ("b.bin", &[0u8; 12][..])]);
        assert!(walk_archive(&tar, "tar.gz", 20, |_, _, _| Ok(())).is_err());
        walk_archive(&tar, "tar.gz", 64 * 1024, |_, _, _| Ok(())).unwrap();
    }

    #[test]
    fn runtime_asset_index_size_is_capped() {
        let mut index = br#"{"schema_version":1,"assets":[]}"#.to_vec();
        index.resize(MAX_ASSET_INDEX as usize + 1, b' ');
        let library = object(TARGETS[0]);
        let archive = zip(&[
            ("custom_runtime.dll", library.as_slice()),
            ("runtime-assets.json", index.as_slice()),
        ]);
        let Err(error) = extract_contents(&archive, "zip", "custom_runtime.dll") else {
            panic!("an oversized index was accepted");
        };
        assert!(
            error.contains("runtime-assets.json exceeds size limit"),
            "{error}"
        );
    }

    #[test]
    fn runtime_asset_entry_count_is_exact() {
        let library = object(TARGETS[0]);
        let worker: &[u8] = b"export const ready = true;\n";
        let names: Vec<String> = (0..126).map(|i| format!("assets/f{i}.mjs")).collect();
        let assets: Vec<serde_json::Value> = names
            .iter()
            .map(|name| serde_json::json!({"path": name, "size": worker.len(), "sha256": hash(worker)}))
            .collect();
        let index = serde_json::to_vec(&serde_json::json!({"schema_version": 1, "assets": assets}))
            .unwrap();
        let mut entries: Vec<(&str, &[u8])> = vec![
            ("custom_runtime.dll", library.as_slice()),
            ("runtime-assets.json", index.as_slice()),
        ];
        entries.extend(names.iter().map(|name| (name.as_str(), worker)));
        // 126 sidecars plus the library and the index make the 128-entry ceiling.
        let contents = extract_contents(&zip(&entries), "zip", "custom_runtime.dll").unwrap();
        assert_eq!(contents.assets.len(), 126);

        // A legacy archive is bounded by the same entry ceiling.
        let legacy: Vec<String> = (0..129).map(|i| format!("docs/{i}.md")).collect();
        let legacy: Vec<(&str, &[u8])> = legacy
            .iter()
            .map(|name| (name.as_str(), &b"x"[..]))
            .collect();
        let error = extract_library(&zip(&legacy), "zip", "custom_runtime.dll").unwrap_err();
        assert!(error.contains("too many archive entries"), "{error}");
    }

    /// Rewrite the uncompressed size that the local and central headers of `name`
    /// claim. The stored bytes do not change, so the entry still holds its real length.
    fn understate_entry(archive: &mut [u8], name: &str, claimed: u32) {
        let name = name.as_bytes();
        let local: Vec<usize> = (0..archive.len())
            .filter(|&at| {
                archive[at..].starts_with(b"PK\x03\x04")
                    && archive
                        .get(at + 30..)
                        .is_some_and(|rest| rest.starts_with(name))
            })
            .collect();
        let central: Vec<usize> = (0..archive.len())
            .filter(|&at| {
                archive[at..].starts_with(b"PK\x01\x02")
                    && archive
                        .get(at + 46..)
                        .is_some_and(|rest| rest.starts_with(name))
            })
            .collect();
        assert!(!local.is_empty() && !central.is_empty());
        for at in local {
            archive[at + 22..at + 26].copy_from_slice(&claimed.to_le_bytes());
        }
        for at in central {
            archive[at + 24..at + 28].copy_from_slice(&claimed.to_le_bytes());
        }
    }

    #[test]
    fn runtime_asset_budget_holds_when_zip_headers_understate_sizes() {
        let library = object(TARGETS[0]);
        let worker = vec![b'x'; 200];
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assets": [{"path": "runtime/main.mjs", "size": worker.len(), "sha256": hash(&worker)}]
        }))
        .unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in [
            ("custom_runtime.dll", library.as_slice()),
            ("runtime-assets.json", index.as_slice()),
            ("runtime/main.mjs", worker.as_slice()),
        ] {
            writer.start_file(name, stored).unwrap();
            writer.write_all(bytes).unwrap();
        }
        let mut archive = writer.finish().unwrap().into_inner();
        // The headers now claim one byte for the sidecar that really holds 200.
        understate_entry(&mut archive, "runtime/main.mjs", 1);

        // The walker sees a small entry, so only the read budget can stop the bytes.
        let exact = (library.len() + index.len() + worker.len()) as u64;
        let limits = ArchiveLimits {
            per_file: u64::MAX,
            total: exact - 1,
        };
        let error = extract_contents_with_limits(&archive, "zip", "custom_runtime.dll", limits)
            .err()
            .unwrap();
        assert!(
            error.contains("expanded archive exceeds size limit"),
            "{error}"
        );

        let limits = ArchiveLimits {
            per_file: u64::MAX,
            total: exact,
        };
        let contents =
            extract_contents_with_limits(&archive, "zip", "custom_runtime.dll", limits).unwrap();
        assert_eq!(contents.assets[0].bytes, worker);
    }

    #[test]
    fn runtime_asset_archives_reject_symlinked_files_and_node_modules_directories() {
        let library = object(TARGETS[0]);
        let worker = b"export {};\n";
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assets": [{"path": "runtime/main.mjs", "size": worker.len(), "sha256": hash(worker)}]
        }))
        .unwrap();
        let mut linked = zip::ZipWriter::new(Cursor::new(Vec::new()));
        linked
            .start_file(
                "custom_runtime.dll",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        linked.write_all(&library).unwrap();
        linked
            .start_file(
                "runtime-assets.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        linked.write_all(&index).unwrap();
        linked
            .add_symlink(
                "runtime/main.mjs",
                "/etc/passwd",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        let archive = linked.finish().unwrap().into_inner();
        let Err(error) = extract_contents(&archive, "zip", "custom_runtime.dll") else {
            panic!("a symlinked sidecar was accepted");
        };
        assert!(error.contains("symlinks are not allowed"), "{error}");

        let empty =
            serde_json::to_vec(&serde_json::json!({"schema_version": 1, "assets": []})).unwrap();
        let mut vendored = zip::ZipWriter::new(Cursor::new(Vec::new()));
        vendored
            .start_file(
                "custom_runtime.dll",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        vendored.write_all(&library).unwrap();
        vendored
            .start_file(
                "runtime-assets.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        vendored.write_all(&empty).unwrap();
        vendored
            .add_directory("node_modules/", zip::write::SimpleFileOptions::default())
            .unwrap();
        let archive = vendored.finish().unwrap().into_inner();
        let Err(error) = extract_contents(&archive, "zip", "custom_runtime.dll") else {
            panic!("a node_modules directory was accepted");
        };
        assert!(error.contains("node_modules"), "{error}");
    }

    #[test]
    fn runtime_asset_digest_mismatch_keeps_the_installed_sidecars() {
        let target = TARGETS[1];
        let worker = b"export {};\n";
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assets": [{"path": "runtime/main.mjs", "size": worker.len(), "sha256": hash(worker)}]
        }))
        .unwrap();
        let (package, value, archive) = fixture_with_entries(
            target,
            &[
                ("runtime-assets.json", &index[..]),
                ("runtime/main.mjs", &worker[..]),
            ],
        );
        let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
        let agent = tempfile::tempdir().unwrap();
        install_archive_at(
            &package,
            &manifest,
            &serde_json::to_vec(&value).unwrap(),
            &archive,
            target,
            agent.path(),
            false,
        )
        .unwrap();
        let metadata = std::fs::read(agent.path().join("native-packages.json")).unwrap();

        // The manifest and archive hashes are consistent. The sidecar keeps its size and changes its bytes.
        let tampered: &[u8] = b"export [];\n";
        assert_eq!(tampered.len(), worker.len());
        let (package, value, archive) = fixture_with_entries(
            target,
            &[
                ("runtime-assets.json", &index[..]),
                ("runtime/main.mjs", tampered),
            ],
        );
        let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
        let error = install_archive_at(
            &package,
            &manifest,
            &serde_json::to_vec(&value).unwrap(),
            &archive,
            target,
            agent.path(),
            true,
        )
        .unwrap_err();
        assert!(
            error.contains("asset runtime/main.mjs: SHA-256 mismatch"),
            "{error}"
        );
        let extensions = agent.path().join("extensions");
        assert_eq!(
            std::fs::read(extensions.join("runtime/main.mjs")).unwrap(),
            worker
        );
        assert_eq!(
            std::fs::read(agent.path().join("native-packages.json")).unwrap(),
            metadata
        );
    }

    #[test]
    fn runtime_asset_tar_archives_reject_duplicate_index_and_declared_entries() {
        let library = object(TARGETS[1]);
        let worker = b"export {};\n";
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assets": [{"path": "runtime/main.mjs", "size": worker.len(), "sha256": hash(worker)}]
        }))
        .unwrap();
        let archive = tgz(&[
            ("libcustom_runtime.so", &library),
            ("runtime-assets.json", &index),
            ("runtime/main.mjs", worker),
            ("runtime/main.mjs", worker),
        ]);
        let Err(error) = extract_contents(&archive, "tar.gz", "libcustom_runtime.so") else {
            panic!("a duplicated sidecar was accepted");
        };
        assert!(error.contains("duplicate declared asset"), "{error}");

        let archive = tgz(&[
            ("libcustom_runtime.so", &library),
            ("runtime-assets.json", &index),
            ("runtime-assets.json", &index),
        ]);
        let Err(error) = extract_contents(&archive, "tar.gz", "libcustom_runtime.so") else {
            panic!("a duplicated index was accepted");
        };
        assert!(error.contains("duplicate runtime-assets.json"), "{error}");
    }

    #[test]
    fn legacy_archives_install_only_the_library_and_keep_their_records_unchanged() {
        for target in TARGETS {
            let (package, value, archive) =
                fixture_with_entries(target, &[("README.md", b"not a sidecar")]);
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            let agent = tempfile::tempdir().unwrap();
            let paths = install_archive_at(
                &package,
                &manifest,
                &serde_json::to_vec(&value).unwrap(),
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            assert_eq!(paths.len(), 1);
            assert!(!agent.path().join("extensions/README.md").exists());
            let records: serde_json::Value = serde_json::from_slice(
                &std::fs::read(agent.path().join("native-packages.json")).unwrap(),
            )
            .unwrap();
            assert!(records[0]["prebuilt"].get("assets").is_none());
        }
    }

    #[test]
    fn all_six_distributions_install_and_record_exact_provenance() {
        for target in TARGETS {
            let (package, value, archive) = fixture(target);
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            let raw = serde_json::to_vec(&value).unwrap();
            let agent = tempfile::tempdir().unwrap();
            let paths = install_archive_at(
                &package,
                &manifest,
                &raw,
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            assert_eq!(paths.len(), 1);
            assert_eq!(std::fs::read(&paths[0]).unwrap(), object(target));
            let records: Vec<crate::install::InstalledNativePackage> = serde_json::from_slice(
                &std::fs::read(agent.path().join("native-packages.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0].name, "independent-plugin");
            assert_eq!(records[0].version, "1.2.3");
            assert_eq!(
                records[0].artifacts,
                [manifest.targets[target].library.path.clone()]
            );
            let origin = records[0].prebuilt.as_ref().unwrap();
            assert_eq!(origin.target, target);
            assert_eq!(origin.manifest_sha256, hash(&raw));
            assert_eq!(origin.repository, package.repository);
            assert_eq!(origin.archive_sha256, hash(&archive));
            assert_eq!(origin.library_sha256, hash(&object(target)));
        }
    }

    #[test]
    fn prebuilt_packages_upgrade_without_force_and_same_version_is_unchanged() {
        for target in TARGETS {
            let (mut package, mut value, archive) = fixture(target);
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            let agent = tempfile::tempdir().unwrap();
            install_archive_at(
                &package,
                &manifest,
                &serde_json::to_vec(&value).unwrap(),
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            package.version = "1.2.4".into();
            value["version"] = serde_json::json!(package.version);
            value["tag"] = serde_json::json!("independent-plugin-v1.2.4");
            let distribution = value.pointer_mut(&format!("/targets/{target}")).unwrap();
            let format = distribution["format"].as_str().unwrap();
            let file = if format == "zip" {
                "package.zip"
            } else {
                "package.tar.gz"
            };
            distribution["url"] = serde_json::json!(release_url(
                &package.repository,
                &package.name,
                &package.version,
                file,
            )
            .unwrap());
            let manifest = serde_json::from_value(value.clone()).unwrap();
            let raw = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                install_archive_at(
                    &package,
                    &manifest,
                    &raw,
                    &archive,
                    target,
                    agent.path(),
                    false,
                )
                .unwrap()
                .len(),
                1
            );
            let metadata = agent.path().join("native-packages.json");
            let registry = std::fs::read(&metadata).unwrap();
            let records: Vec<crate::install::InstalledNativePackage> =
                serde_json::from_slice(&registry).unwrap();
            assert_eq!(records[0].version, "1.2.4");
            assert!(install_archive_at(
                &package,
                &manifest,
                &raw,
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap()
            .is_empty());
            assert_eq!(std::fs::read(&metadata).unwrap(), registry);
        }
    }

    #[test]
    fn bad_release_identity_hash_abi_and_target_leave_installed_version_intact() {
        let target = TARGETS[0];
        let (package, good, archive) = fixture(target);
        let manifest: Manifest = serde_json::from_value(good.clone()).unwrap();
        let agent = tempfile::tempdir().unwrap();
        let path = install_archive_at(
            &package,
            &manifest,
            &serde_json::to_vec(&good).unwrap(),
            &archive,
            target,
            agent.path(),
            false,
        )
        .unwrap()
        .remove(0);
        let original_record = std::fs::read(agent.path().join("native-packages.json")).unwrap();
        let mutations = [
            ("/schema_version",serde_json::json!(99)), ("/name",serde_json::json!("other-plugin")),
            ("/version",serde_json::json!("9.9.9")), ("/tag",serde_json::json!("v1.2.3")),
            ("/repository",serde_json::json!("https://github.com/other/repo")),
            ("/sdk/plugin_abi",serde_json::json!(99)),
            ("/targets/x86_64-pc-windows-msvc/sha256",serde_json::json!("0".repeat(64))),
            ("/targets/x86_64-pc-windows-msvc/library/sha256",serde_json::json!("0".repeat(64))),
            ("/targets/x86_64-pc-windows-msvc/library/path",serde_json::json!("../custom_runtime.dll")),
            ("/targets/x86_64-pc-windows-msvc/url",serde_json::json!("https://github.com/other/repo/releases/download/independent-plugin-v1.2.3/package.zip")),
            ("/targets/x86_64-pc-windows-msvc/size",serde_json::json!(archive.len()+1)),
        ];
        for (pointer, value) in mutations {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            let bad_manifest = serde_json::from_value(bad.clone()).unwrap();
            assert!(
                install_archive_at(
                    &package,
                    &bad_manifest,
                    &serde_json::to_vec(&bad).unwrap(),
                    &archive,
                    target,
                    agent.path(),
                    true
                )
                .is_err(),
                "{pointer}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), object(target));
            assert_eq!(
                std::fs::read(agent.path().join("native-packages.json")).unwrap(),
                original_record
            );
        }
        assert!(install_archive_at(
            &package,
            &manifest,
            &[],
            &archive,
            TARGETS[1],
            agent.path(),
            true
        )
        .is_err());
    }

    #[test]
    fn same_abi_but_wrong_binary_architecture_is_rejected_before_activation() {
        let (package, mut value, _) = fixture(TARGETS[0]);
        let wrong = object(TARGETS[2]);
        let archive = zip(&[("custom_runtime.dll", &wrong)]);
        let distribution = value
            .pointer_mut("/targets/x86_64-pc-windows-msvc")
            .unwrap();
        distribution["size"] = serde_json::json!(archive.len());
        distribution["sha256"] = serde_json::json!(hash(&archive));
        distribution["library"]["sha256"] = serde_json::json!(hash(&wrong));
        let manifest = serde_json::from_value(value).unwrap();
        let agent = tempfile::tempdir().unwrap();
        assert!(install_archive_at(
            &package,
            &manifest,
            &[],
            &archive,
            TARGETS[0],
            agent.path(),
            false
        )
        .unwrap_err()
        .contains("shared library"));
        assert!(!agent.path().join("native-packages.json").exists());
        assert!(!agent.path().join("extensions").exists());
    }

    #[test]
    fn archives_cannot_traverse_retain_links_or_substitute_another_library() {
        let library = object(TARGETS[0]);
        assert!(
            extract_library(&zip(&[("../escape.dll", &library)]), "zip", "escape.dll").is_err()
        );
        assert!(
            extract_library(&zip(&[("C:\\escape.dll", &library)]), "zip", "escape.dll").is_err()
        );
        assert!(extract_library(&zip(&[("other.dll", &library)]), "zip", "declared.dll").is_err());
        let mut linked = zip::ZipWriter::new(Cursor::new(Vec::new()));
        linked
            .add_symlink(
                "declared.dll",
                "outside.dll",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        assert!(extract_library(
            &linked.finish().unwrap().into_inner(),
            "zip",
            "declared.dll"
        )
        .is_err());
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_link(&mut header, "declared.so", "outside.so")
            .unwrap();
        assert!(extract_library(
            &tar.into_inner().unwrap().finish().unwrap(),
            "tar.gz",
            "declared.so"
        )
        .is_err());
    }

    #[test]
    fn object_headers_cannot_cross_platform_or_architecture() {
        for target in TARGETS {
            assert!(validate_library_target(&object(target), target).is_ok());
            for other in TARGETS {
                if other != target {
                    // ELF headers do not encode the libc choice, so GNU and
                    // musl x86_64 objects are intentionally indistinguishable
                    // to this architecture check. Exact target manifests still
                    // keep those distributions separate.
                    if target.starts_with("x86_64-unknown-linux-")
                        && other.starts_with("x86_64-unknown-linux-")
                    {
                        continue;
                    }
                    assert!(validate_library_target(&object(other), target).is_err());
                }
            }
        }
        assert!(validate_library_target(&[0u8; 8], TARGETS[0]).is_err());
        let mut damaged = object(TARGETS[0]);
        damaged[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_library_target(&damaged, TARGETS[0]).is_err());
    }

    #[test]
    fn exact_versions_ignore_yanked_and_prerelease_latest() {
        let versions = [
            RegistryVersion {
                num: "1.0.0".into(),
                yanked: false,
            },
            RegistryVersion {
                num: "9.0.0".into(),
                yanked: true,
            },
            RegistryVersion {
                num: "2.0.0-beta.1".into(),
                yanked: false,
            },
        ];
        assert_eq!(select_version(&versions, None).unwrap(), "1.0.0");
        assert_eq!(
            select_version(&versions, Some("2.0.0-beta.1")).unwrap(),
            "2.0.0-beta.1"
        );
        assert!(select_version(&versions, Some("9.0.0")).is_err());
        assert!(select_version(&versions, Some("^1")).is_err());
    }

    #[test]
    fn github_case_and_encoded_semver_build_metadata_preserve_release_identity() {
        let (mut package, mut value, _) = fixture(TARGETS[0]);
        package.version = "1.2.3+build.1".into();
        value["version"] = serde_json::json!(package.version);
        value["tag"] = serde_json::json!("independent-plugin-v1.2.3+build.1");
        value["repository"] = serde_json::json!("https://github.com/Publisher/MonoRepo");
        value["targets"][TARGETS[0]]["url"] = serde_json::json!("https://github.com/Publisher/MonoRepo/releases/download/independent-plugin-v1.2.3%2Bbuild.1/package.zip");
        let manifest = serde_json::from_value(value).unwrap();
        assert!(validate_manifest(&manifest, &package, TARGETS[0]).is_ok());
        assert_eq!(
            canonical_repository("Publisher/MonoRepo").unwrap(),
            package.repository
        );
    }

    #[test]
    fn repository_urls_are_explicit_and_release_tags_use_package_versions() {
        assert_eq!(
            canonical_repository("git+https://github.com/publisher/monorepo.git").unwrap(),
            "https://github.com/publisher/monorepo"
        );
        assert_eq!(
            canonical_repository("https://github.com/publisher/monorepo/tree/main/packages/plugin")
                .unwrap(),
            "https://github.com/publisher/monorepo"
        );
        for invalid in [
            "https://gitlab.com/x/y",
            "https://user@github.com/x/y",
            "https://github.com/x",
            "https://github.com/x/y/releases/latest",
            "https://github.com:8443/x/y",
        ] {
            assert!(canonical_repository(invalid).is_err());
        }
        assert_eq!(release_url("https://github.com/publisher/monorepo","plugin-one","1.2.3","rpi-extension.json").unwrap(),"https://github.com/publisher/monorepo/releases/download/plugin-one-v1.2.3/rpi-extension.json");
        assert!(release_url(
            "https://github.com/publisher/monorepo",
            "plugin-two",
            "4.5.6",
            "rpi-extension.json"
        )
        .unwrap()
        .contains("plugin-two-v4.5.6"));
    }

    fn one_response(body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        response_with_status("200 OK", body)
    }

    fn response_with_status(
        status: &'static str,
        body: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
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
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, thread)
    }

    #[tokio::test]
    async fn downloaded_manifest_and_archive_install_into_an_isolated_profile() {
        let target = host_target().unwrap();
        let (package, value, archive) = fixture(target);
        let raw = serde_json::to_vec(&value).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (url, server) = one_response(raw.clone());
        let response = client.get(url).send().await.unwrap();
        let (manifest, received) = read_manifest_response(response, &package).await.unwrap();
        server.join().unwrap();
        assert_eq!(received, raw);
        let distribution = validate_manifest(&manifest, &package, target).unwrap();
        let (url, server) = one_response(archive);
        let response = client.get(url).send().await.unwrap();
        let downloaded = body_limited(response, distribution.size).await.unwrap();
        server.join().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let paths = install_archive_at(
            &package,
            &manifest,
            &received,
            &downloaded,
            target,
            profile.path(),
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), object(target));
        assert!(profile.path().join("native-packages.json").is_file());
    }

    #[tokio::test]
    async fn missing_invalid_and_oversized_manifests_fail_before_installation() {
        let (package, _, _) = fixture(TARGETS[0]);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (status, body, expected) in [
            (
                "404 Not Found",
                b"missing".to_vec(),
                "rpi-extension.json is missing",
            ),
            ("403 Forbidden", b"denied".to_vec(), "403 Forbidden"),
            ("200 OK", b"not json".to_vec(), "invalid rpi-extension.json"),
            (
                "200 OK",
                vec![b' '; MAX_MANIFEST as usize + 1],
                "size limit",
            ),
        ] {
            let (url, server) = response_with_status(status, body);
            let response = client.get(url).send().await.unwrap();
            let error = read_manifest_response(response, &package)
                .await
                .err()
                .unwrap();
            assert!(error.contains(expected), "{error}");
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn explicit_repository_and_version_do_not_require_a_registry_record() {
        let client = reqwest::Client::new();
        let resolved = resolve_package(
            &client,
            "http://127.0.0.1:1",
            "private-extension",
            Some("2.3.4"),
            Some("https://github.com/custom-owner/custom-repo.git"),
        )
        .await
        .unwrap();
        assert_eq!(
            resolved.repository,
            "https://github.com/custom-owner/custom-repo"
        );
        assert_eq!(resolved.name, "private-extension");
        assert_eq!(resolved.version, "2.3.4");
        assert!(release_url(
            &resolved.repository,
            &resolved.name,
            &resolved.version,
            "rpi-extension.json"
        )
        .unwrap()
        .contains("custom-repo/releases/download/private-extension-v2.3.4/"));
    }

    #[tokio::test]
    async fn custom_repository_versions_are_scoped_to_the_package() {
        let refs = serde_json::to_vec(&serde_json::json!([
            {"ref":"refs/tags/independent-plugin-v1.2.3"},
            {"ref":"refs/tags/independent-plugin-v1.4.0"},
            {"ref":"refs/tags/other-plugin-v99.0.0"},
            {"ref":"refs/tags/independent-plugin-v2.0.0-beta.1"}
        ]))
        .unwrap();
        let (api, server) = one_response(refs);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            repository_version(
                &client,
                &api,
                "independent-plugin",
                "https://github.com/publisher/monorepo"
            )
            .await
            .unwrap(),
            "1.4.0"
        );
        assert!(server.join().unwrap().starts_with(
            "GET /repos/publisher/monorepo/git/matching-refs/tags/independent-plugin-v "
        ));
    }

    #[tokio::test]
    async fn registry_discovery_uses_repository_metadata_and_downloads_are_bounded() {
        let payload = serde_json::to_vec(&serde_json::json!({"crate":{"id":"independent-plugin","repository":"https://github.com/publisher/monorepo.git"},"versions":[{"num":"1.2.3","yanked":false},{"num":"3.0.0-beta.1","yanked":false},{"num":"9.0.0","yanked":true}]})).unwrap();
        let (url, server) = one_response(payload);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let package = resolve_package(&client, &url, "independent-plugin", None, None)
            .await
            .unwrap();
        assert_eq!(package.repository, "https://github.com/publisher/monorepo");
        assert_eq!(package.version, "1.2.3");
        assert!(server
            .join()
            .unwrap()
            .starts_with("GET /independent-plugin "));
        let (url, server) = one_response(vec![0u8; 32]);
        let response = client.get(url).send().await.unwrap();
        assert!(body_limited(response, 8).await.is_err());
        server.join().unwrap();
    }
}
