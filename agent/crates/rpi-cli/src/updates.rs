//! Lightweight update discovery and update commands.
//!
//! Startup checks are deliberately best-effort: callers decide when to run
//! them, each registry request uses a short timeout, and cached values are
//! retained only as a fallback for temporary registry failures.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};

const CACHE_FILE: &str = "update-check.json";
const CACHE_FALLBACK_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
const REQUEST_TIMEOUT_MS: u64 = 1800;
const UPDATE_CHECK_CONCURRENCY: usize = 4;
const GITHUB_RELEASES_API: &str =
    "https://api.github.com/repos/bigfish1913/pi-rust/releases?per_page=100";
/// The single-release endpoint for the newest non-draft, non-prerelease tag.
/// Authoritative version source for the stable CLI channel.
const GITHUB_RELEASES_LATEST_API: &str =
    "https://api.github.com/repos/bigfish1913/pi-rust/releases/latest";
const STAGED_RPI_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_STAGED_RPI_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_SELF_UPDATE_STATUS_BYTES: u64 = 64 * 1024;
const MAX_SELF_UPDATE_STATUS_MESSAGE_CHARS: usize = 2 * 1024;
/// How long a running self-update may stay in a non-terminal state before a
/// later process treats it as debris and prunes it. A `cargo install` compiles
/// from source, so the window is generous; a helper that outlives it is stuck,
/// not slow.
const STALE_SELF_UPDATE_STATUS_TTL_MS: i64 = 60 * 60 * 1000;
/// How long a completed update keeps its status record. `rpi update` prints
/// that path, so the file has to outlive the command that named it by long
/// enough to actually be read; after that it is just clutter.
const COMPLETED_SELF_UPDATE_STATUS_TTL_MS: i64 = 24 * 60 * 60 * 1000;
static UPDATE_CACHE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SELF_UPDATE_STATUS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateWarning {
    pub message: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub notices: Vec<UpdateNotice>,
    pub warnings: Vec<UpdateWarning>,
}

impl UpdateReport {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.warnings.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCache {
    /// Legacy aggregate timestamp retained for backward-compatible cache reads.
    checked_at: i64,
    rpi_latest: Option<String>,
    #[serde(default)]
    native_packages: BTreeMap<String, String>,
    #[serde(default)]
    freshness: UpdateCacheFreshness,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCacheFreshness {
    rpi: i64,
    #[serde(default)]
    native_packages: BTreeMap<String, i64>,
}

type RegistryCacheUpdate = (BTreeMap<String, String>, BTreeMap<String, i64>);

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_version: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateChannel {
    Stable,
    Preview,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelfUpdateStatus {
    state: String,
    #[serde(default)]
    message: String,
    /// The staging directory this update was installed into. Written by the
    /// Windows helper; the older `preparing` records lack it, which is why an
    /// unmapped staging directory is aged out instead of matched by name.
    #[serde(default)]
    staging: Option<PathBuf>,
    #[serde(default)]
    updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryLookup {
    Found(String),
    Missing,
    TransientFailure,
}

/// Best-effort startup check for the rpi release and installed Rust extensions.
pub async fn check_startup(cwd: &Path) -> UpdateReport {
    let _ = cwd;
    check_rpi_startup().await
}

/// Check the rpi release and every installed Rust cdylib extension.
///
/// Native-only: the npm/Git package halves of the old scope went away with the
/// Pi compatibility layer, so this never shells out to a package manager.
pub async fn check_rpi_startup() -> UpdateReport {
    if update_checks_disabled() {
        return report_without_remote_checks();
    }
    check_startup_with_client(startup_http_client()).await
}

fn update_checks_disabled() -> bool {
    crate::args::offline_env_enabled() || std::env::var_os("RPI_DISABLE_UPDATE_CHECK").is_some()
}

/// Report the outcome of a self-update that ran in a previous process.
fn report_without_remote_checks() -> UpdateReport {
    let warnings = crate::config::agent_dir()
        .ok()
        .map(|agent_dir| consume_self_update_statuses(&agent_dir))
        .unwrap_or_default();
    UpdateReport {
        notices: Vec::new(),
        warnings,
    }
}

fn startup_http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .ok()
}

async fn check_startup_with_client(client: Option<reqwest::Client>) -> UpdateReport {
    let agent_dir = match crate::config::agent_dir() {
        Ok(dir) => dir,
        Err(_) => return UpdateReport::default(),
    };
    let warnings = consume_self_update_statuses(&agent_dir);
    let cache_path = agent_dir.join(CACHE_FILE);
    let previous_cache = read_cache(&cache_path);

    // Installed Rust extensions come from `rpi install`'s registry, so this
    // path stays independent of any Pi package resource discovery.
    let native_checks = crate::install::installed_native_packages()
        .into_iter()
        .filter(|package| package.supports_registry_update())
        .map(|package| {
            let client = client.clone();
            async move {
                let latest = match client.as_ref() {
                    Some(client) if package.prebuilt.is_some() => {
                        match crate::prebuilt_extensions::latest_compatible_version(
                            client, &package,
                        )
                        .await
                        {
                            Ok(Some(version)) => RegistryLookup::Found(version),
                            Ok(None) => RegistryLookup::Missing,
                            Err(_) => RegistryLookup::TransientFailure,
                        }
                    }
                    Some(client) => fetch_crates_latest(client, &package.name).await,
                    None => RegistryLookup::TransientFailure,
                };
                (native_cache_key(&package), latest)
            }
        });
    let rpi_check = async {
        Some(match client.as_ref() {
            Some(client) => fetch_rpi_latest(client).await,
            None => RegistryLookup::TransientFailure,
        })
    };
    let (rpi_result, native_results) = tokio::join!(rpi_check, collect_bounded(native_checks));

    let checked_at = now_ms();
    let mut rpi_update = None;
    if let Some(rpi_result) = rpi_result {
        let cached_at = previous_cache
            .as_ref()
            .map(rpi_cache_checked_at)
            .unwrap_or_default();
        let fallback_allowed = timestamp_is_fresh(cached_at, checked_at);
        let (latest, used_fallback) = lookup_with_cache_fallback(
            rpi_result,
            previous_cache
                .as_ref()
                .and_then(|cache| cache.rpi_latest.as_deref()),
            fallback_allowed,
        );
        let value_checked_at =
            resolved_checked_at(latest.as_deref(), used_fallback, cached_at, checked_at);
        rpi_update = Some((latest, value_checked_at));
    }

    let legacy_checked_at = previous_cache
        .as_ref()
        .map(|cache| cache.checked_at)
        .unwrap_or_default();
    let native_packages = results_with_individual_cache_fallback(
        native_results,
        previous_cache.as_ref().map(|cache| &cache.native_packages),
        previous_cache
            .as_ref()
            .map(|cache| &cache.freshness.native_packages),
        legacy_checked_at,
        checked_at,
    );

    let cache = merge_and_write_cache(&cache_path, checked_at, rpi_update, native_packages);
    let mut report = report_from_cache(&cache);
    report.warnings = warnings;
    report
}

async fn collect_bounded<I, F, T>(checks: I) -> Vec<T>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = T>,
{
    stream::iter(checks)
        .buffer_unordered(UPDATE_CHECK_CONCURRENCY)
        .collect()
        .await
}

fn rpi_cache_checked_at(cache: &UpdateCache) -> i64 {
    if cache.freshness.rpi > 0 {
        cache.freshness.rpi
    } else {
        cache.checked_at
    }
}

fn entry_cache_checked_at(
    freshness: Option<&BTreeMap<String, i64>>,
    key: &str,
    legacy_checked_at: i64,
) -> i64 {
    freshness
        .and_then(|values| values.get(key).copied())
        .filter(|checked_at| *checked_at > 0)
        .unwrap_or(legacy_checked_at)
}

fn timestamp_is_fresh(checked_at: i64, now: i64) -> bool {
    let age = now.saturating_sub(checked_at);
    checked_at > 0 && (0..=CACHE_FALLBACK_MAX_AGE_MS).contains(&age)
}

fn resolved_checked_at(
    value: Option<&str>,
    used_fallback: bool,
    cached_at: i64,
    checked_at: i64,
) -> i64 {
    if value.is_none() {
        0
    } else if used_fallback {
        cached_at
    } else {
        checked_at
    }
}

fn results_with_individual_cache_fallback(
    results: Vec<(String, RegistryLookup)>,
    cached: Option<&BTreeMap<String, String>>,
    freshness: Option<&BTreeMap<String, i64>>,
    legacy_checked_at: i64,
    checked_at: i64,
) -> RegistryCacheUpdate {
    let mut values = BTreeMap::new();
    let mut checked_at_values = BTreeMap::new();
    for (name, result) in results {
        let cached_value = cached.and_then(|cached| cached.get(&name).map(String::as_str));
        let cached_at = entry_cache_checked_at(freshness, &name, legacy_checked_at);
        let (latest, used_fallback) = lookup_with_cache_fallback(
            result,
            cached_value,
            timestamp_is_fresh(cached_at, checked_at),
        );
        if let Some(latest) = latest {
            checked_at_values.insert(
                name.clone(),
                resolved_checked_at(Some(&latest), used_fallback, cached_at, checked_at),
            );
            values.insert(name, latest);
        }
    }
    (values, checked_at_values)
}

fn merge_and_write_cache(
    path: &Path,
    checked_at: i64,
    rpi_update: Option<(Option<String>, i64)>,
    native_packages: RegistryCacheUpdate,
) -> UpdateCache {
    let _guard = UPDATE_CACHE_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cache = read_cache(path).unwrap_or_default();
    if cache.checked_at <= 0 {
        cache.checked_at = checked_at;
    }
    if let Some((latest, latest_checked_at)) = rpi_update {
        cache.rpi_latest = latest;
        cache.freshness.rpi = latest_checked_at;
    }
    cache.native_packages = native_packages.0;
    cache.freshness.native_packages = native_packages.1;
    let _ = write_cache(path, &cache);
    cache
}

fn lookup_with_cache_fallback(
    result: RegistryLookup,
    cached: Option<&str>,
    fallback_allowed: bool,
) -> (Option<String>, bool) {
    match result {
        RegistryLookup::Found(version) => (Some(version), false),
        RegistryLookup::Missing => (None, false),
        RegistryLookup::TransientFailure if fallback_allowed => {
            (cached.map(str::to_owned), cached.is_some())
        }
        RegistryLookup::TransientFailure => (None, false),
    }
}
fn native_cache_key(package: &crate::install::InstalledNativePackage) -> String {
    match &package.prebuilt {
        Some(source) => format!(
            "prebuilt:{}:{}#{}:{}",
            if source.custom_repository {
                "github"
            } else {
                "crates"
            },
            source.repository,
            package.name,
            source.target
        ),
        None => package.name.clone(),
    }
}

fn report_from_cache(cache: &UpdateCache) -> UpdateReport {
    let mut notices = Vec::new();
    if let Some(latest) = cache.rpi_latest.as_deref() {
        if is_newer(crate::VERSION, latest) {
            notices.push(UpdateNotice {
                name: "rpi".into(),
                current: crate::VERSION.into(),
                latest: latest.into(),
                command: "rpi update".into(),
            });
        }
    }
    for package in crate::install::installed_native_packages() {
        if !package.supports_registry_update() {
            continue;
        }
        let Some(latest) = cache.native_packages.get(&native_cache_key(&package)) else {
            continue;
        };
        if is_newer(&package.version, latest) {
            notices.push(UpdateNotice {
                name: package.name.clone(),
                current: package.version,
                latest: latest.clone(),
                command: format!("rpi extension update {}", package.name),
            });
        }
    }
    UpdateReport {
        notices,
        warnings: Vec::new(),
    }
}

fn normalize_windows_path(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(local);
        }
    }
    path
}

fn windows_paths_equal(left: &Path, right: &Path) -> bool {
    let left = normalize_windows_path(left.to_path_buf());
    let right = normalize_windows_path(right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    } else {
        left == right
    }
}

async fn fetch_rpi_latest(client: &reqwest::Client) -> RegistryLookup {
    fetch_stable_from_github(client, GITHUB_RELEASES_LATEST_API).await
}

/// The stable version a `/releases/latest` body names, or `None` when the body
/// is not a usable stable release.
///
/// `/releases/latest` already excludes drafts and prereleases, but the flags are
/// re-checked here so a GitHub API behaviour change cannot silently hand the
/// stable channel a prerelease. This is deliberately separate from
/// `latest_preview_version`, which selects prereleases from the full list.
fn stable_version_from_release(release: &GithubRelease) -> Option<String> {
    if release.prerelease || release.draft {
        return None;
    }
    let version = release.tag_name.strip_prefix('v')?;
    let parsed = semver::Version::parse(version).ok()?;
    parsed.pre.is_empty().then(|| version.to_string())
}

async fn fetch_stable_from_github(client: &reqwest::Client, url: &str) -> RegistryLookup {
    let response = match client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    // Missing, unavailable or malformed release metadata must not invalidate
    // a fresh cached answer. A release may be temporarily unavailable while
    // publishing; offline/cache age policy stays with the caller.
    if !response.status().is_success() {
        return RegistryLookup::TransientFailure;
    }
    let release = match response.json::<GithubRelease>().await {
        Ok(release) => release,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    // A body that is not a usable stable release is an unanswered lookup too,
    // for the same reason: keep a fresh cache able to serve the request.
    match stable_version_from_release(&release) {
        Some(version) => RegistryLookup::Found(version),
        None => RegistryLookup::TransientFailure,
    }
}

fn latest_preview_version(releases: impl IntoIterator<Item = GithubRelease>) -> RegistryLookup {
    releases
        .into_iter()
        .filter(|release| release.prerelease && !release.draft)
        .filter_map(|release| {
            let version = release.tag_name.strip_prefix('v')?.to_string();
            let parsed = semver::Version::parse(&version).ok()?;
            (!parsed.pre.is_empty()).then_some((parsed, version))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, version)| RegistryLookup::Found(version))
        .unwrap_or(RegistryLookup::Missing)
}

async fn fetch_rpi_preview_latest(client: &reqwest::Client) -> RegistryLookup {
    let response = match client
        .get(GITHUB_RELEASES_API)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return RegistryLookup::Missing;
    }
    if !response.status().is_success() {
        return RegistryLookup::TransientFailure;
    }
    let releases = match response.json::<Vec<GithubRelease>>().await {
        Ok(releases) => releases,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    latest_preview_version(releases)
}

async fn fetch_crates_latest(client: &reqwest::Client, name: &str) -> RegistryLookup {
    fetch_crates_url(client, &format!("https://crates.io/api/v1/crates/{name}")).await
}

async fn fetch_crates_url(client: &reqwest::Client, url: &str) -> RegistryLookup {
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    if matches!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return RegistryLookup::Missing;
    }
    if !response.status().is_success() {
        return RegistryLookup::TransientFailure;
    }
    match response.json::<CratesResponse>().await {
        Ok(response) => RegistryLookup::Found(response.crate_info.max_version),
        Err(_) => RegistryLookup::TransientFailure,
    }
}

/// Compare complete semantic versions, including prerelease precedence.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |value: &str| semver::Version::parse(value.trim().trim_start_matches('v')).ok();
    match (parse(current), parse(latest)) {
        (Some(current), Some(latest)) => latest.cmp_precedence(&current).is_gt(),
        _ => false,
    }
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &UpdateCache) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let data = serde_json::to_vec_pretty(cache).map_err(|error| error.to_string())?;
    crate::config::atomic_write(path, &data).map_err(|error| error.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn print_startup_notices(report: &UpdateReport) {
    for warning in &report.warnings {
        eprintln!("warning: {} Run `{}`.", warning.message, warning.command);
    }
    for notice in &report.notices {
        eprintln!(
            "Update available: {} {} -> {}. Run `{}`.",
            notice.name, notice.current, notice.latest, notice.command
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelfUpdateArgs {
    Help,
    Run {
        force: bool,
        build: bool,
        channel: UpdateChannel,
        cdn_base: Option<reqwest::Url>,
    },
    Usage,
}

/// Parse `rpi update`'s own arguments after the global `--offline` flag has been
/// stripped.
fn parse_self_update_args(args: &[String]) -> SelfUpdateArgs {
    let mut force = false;
    let mut build = false;
    let mut channel = UpdateChannel::Stable;
    let mut cdn = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return SelfUpdateArgs::Help,
            "--force" | "-f" => force = true,
            "--build" => build = true,
            "--cdn" => {
                i += 1;
                let Some(value) = args.get(i).filter(|value| !value.starts_with('-')) else {
                    return SelfUpdateArgs::Usage;
                };
                cdn = Some(value.as_str());
            }
            "preview" if channel == UpdateChannel::Stable => channel = UpdateChannel::Preview,
            _ => return SelfUpdateArgs::Usage,
        }
        i += 1;
    }
    let cdn_base = match crate::cdn::resolve_base(cdn) {
        Ok(base) => base,
        Err(_) => return SelfUpdateArgs::Usage,
    };
    SelfUpdateArgs::Run {
        force,
        build,
        channel,
        cdn_base,
    }
}

fn print_self_update_help() {
    println!(
        "Usage: rpi update [preview] [--force] [--offline] [--build] [--cdn <URL>]\n\n\
         Update the rpi CLI using verified GitHub prebuilt releases.\n\
         The optional `preview` channel installs the latest GitHub prerelease;\n\
         it is never selected by the normal update check.\n\n\
         Options:\n  \
         preview    use the GitHub prerelease channel instead of stable releases\n  \
         --build    explicitly compile with Cargo instead of downloading a prebuilt binary\n  \
         --force    reinstall even when this rpi is already the latest release\n  \
         --offline  do not touch the network; report the last known answer only"
    );
}

/// The latest release this process already knows about.
///
/// `registry_lookup: true` means a live request just answered *in this process*
/// from the official GitHub release channel, so using it to short-circuit
/// cannot hide a newer release. A value read from the
/// 6-hour cache is only trusted for the offline report, where there is no
/// alternative and the output says so.
fn known_latest_rpi(registry_lookup: Option<&str>, cached: Option<&str>) -> Option<(String, bool)> {
    registry_lookup
        .map(|latest| (latest.to_string(), false))
        .or_else(|| cached.map(|latest| (latest.to_string(), true)))
}

async fn lookup_latest_rpi() -> Option<(String, bool)> {
    lookup_latest_rpi_from(GITHUB_RELEASES_LATEST_API).await
}

/// Resolve the latest stable version and report whether it came from a live
/// answer.
///
/// The endpoint is a parameter so the whole decision — including `verified`
/// and the cache fallback — can be exercised offline.
async fn lookup_latest_rpi_from(github_latest_url: &str) -> Option<(String, bool)> {
    let cache_path = crate::config::agent_dir()
        .ok()
        .map(|dir| dir.join(CACHE_FILE));
    let cached = cache_path.as_deref().and_then(read_cache);
    let result = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
    {
        Ok(client) => fetch_stable_from_github(&client, github_latest_url).await,
        Err(_) => RegistryLookup::TransientFailure,
    };
    // Only a live answer from an authoritative source counts as verified; a
    // value that came back from the cache means every request failed and the
    // version is up to six hours old. Only GitHub supplies live CLI versions.
    let verified = matches!(result, RegistryLookup::Found(_));
    let previous = cached
        .as_ref()
        .filter(|cache| timestamp_is_fresh(rpi_cache_checked_at(cache), now_ms()))
        .and_then(|cache| cache.rpi_latest.as_deref());
    let (latest, _) = lookup_with_cache_fallback(result, previous, true);
    latest.map(|latest| (latest, verified))
}

/// `rpi update` is synchronous, so its registry lookup runs on its own thread and
/// current-thread runtime. That also keeps it safe to call from inside a Tokio
/// runtime, which is where the CLI enters it.
fn lookup_latest_rpi_blocking() -> Option<(String, bool)> {
    std::thread::Builder::new()
        .name("rpi-update-latest-lookup".to_string())
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(lookup_latest_rpi())
        })
        .ok()?
        .join()
        .ok()?
}

async fn lookup_latest_preview() -> Option<(String, bool)> {
    let result = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
    {
        Ok(client) => fetch_rpi_preview_latest(&client).await,
        Err(_) => RegistryLookup::TransientFailure,
    };
    let verified = matches!(result, RegistryLookup::Found(_));
    match result {
        RegistryLookup::Found(version) => Some((version, verified)),
        _ => None,
    }
}

fn lookup_latest_preview_blocking() -> Option<(String, bool)> {
    std::thread::Builder::new()
        .name("rpi-update-preview-lookup".to_string())
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(lookup_latest_preview())
        })
        .ok()?
        .join()
        .ok()?
}

/// The newest version already staged in a `pending-*` directory.
///
/// A staged update is a fully validated binary waiting for the running process
/// to exit. Re-running `rpi update` while one is queued must not start a second
/// compile, so the staged version joins the up-to-date comparison.
fn staged_pending_version(update_root: &Path, channel: UpdateChannel) -> Option<semver::Version> {
    let entries = std::fs::read_dir(update_root).ok()?;
    let mut newest: Option<semver::Version> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("pending-") else {
            continue;
        };
        // `tempfile` builds the name from `[A-Za-z0-9]`; anything else is not
        // ours, so it is left alone rather than probed.
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            continue;
        }
        let staging = update_root.join(name);
        if validated_real_directory(&staging, "self-update staging directory").is_err() {
            continue;
        }
        let Ok(version) = validate_rpi_executable_version(
            &staging.join("bin").join(staged_rpi_file_name()),
            update_root,
            crate::VERSION,
            "staged rpi",
        ) else {
            continue;
        };
        if (channel == UpdateChannel::Stable && !version.pre.is_empty())
            || (channel == UpdateChannel::Preview && version.pre.is_empty())
        {
            continue;
        }
        if newest.as_ref().map_or(true, |newest| version > *newest) {
            newest = Some(version);
        }
    }
    newest
}

fn staged_rpi_file_name() -> &'static str {
    if cfg!(windows) {
        "rpi.exe"
    } else {
        crate::APP_NAME
    }
}

/// Reclaim debris and surface a previous run's failure, without a network
/// round trip.
///
/// `rpi update` is the command that creates `self-update` records and staging
/// directories, so it is also the right place to clean them up: leaving it to
/// the startup check means a machine that only ever runs `rpi chat` keeps the
/// debris from every interrupted update until the next start.
fn reclaim_self_update_artifacts() {
    let Ok(agent_dir) = crate::config::agent_dir() else {
        return;
    };
    for warning in consume_self_update_statuses(&agent_dir) {
        eprintln!("warning: {} Run `{}`.", warning.message, warning.command);
    }
}

/// Download a verified CLI release by default; --build explicitly selects Cargo.
/// Both paths use the same staging validation and transactional replacement.
pub fn run_self_update(args: &[String]) -> i32 {
    let offline = crate::args::normalize_offline_mode(args);
    let args = crate::args::without_offline_flag(args);
    let (force, build, channel, cdn_base) = match parse_self_update_args(&args) {
        SelfUpdateArgs::Help => {
            print_self_update_help();
            return 0;
        }
        SelfUpdateArgs::Run {
            force,
            build,
            channel,
            cdn_base,
        } => (force, build, channel, cdn_base),
        SelfUpdateArgs::Usage => {
            eprintln!(
                "error: `rpi update` accepts only `preview`, `--force`, `--offline`, `--build` and `--cdn <URL>`"
            );
            return 2;
        }
    };

    if offline {
        reclaim_self_update_artifacts();
        if channel == UpdateChannel::Preview {
            println!(
                "rpi preview update skipped: offline mode is enabled;\n\
                 run `rpi update preview` without --offline to query GitHub."
            );
            return 0;
        }
        // The cache is the only source available without a network round trip,
        // so it may be stale — say so rather than assert a version.
        match known_latest_rpi(None, cached_latest_rpi().as_deref()) {
            Some((latest, true)) if is_newer(crate::VERSION, &latest) => println!(
                "rpi {} -> {} is available (offline: last known, not verified)\n\
                 Run `rpi update` without --offline to install it.",
                crate::VERSION,
                latest
            ),
            Some(_) => println!(
                "rpi is up to date with the last known release ({})\n\
                 Offline mode is enabled; the registry was not contacted.",
                crate::VERSION
            ),
            None => println!(
                "rpi update skipped: offline mode is enabled and no cached release is known"
            ),
        }
        return 0;
    }

    // Without this gate `rpi update` at the newest release still runs a full
    // `cargo install` and schedules a binary replacement for the version that
    // is already installed — which is exactly what makes a working update look
    // broken. `--force` keeps an explicit reinstall available.
    let release = if !build || !force {
        match channel {
            UpdateChannel::Stable => lookup_latest_rpi_blocking(),
            UpdateChannel::Preview => lookup_latest_preview_blocking(),
        }
    } else {
        None
    };
    if !force {
        reclaim_self_update_artifacts();
        let staged = pending_staged_version(channel);
        let (latest, verified) = match release.clone() {
            Some(found) => found,
            None if staged.is_some() => {
                println!(
                    "could not reach the latest {} release; using the already staged update",
                    if channel == UpdateChannel::Preview {
                        "preview"
                    } else {
                        "stable"
                    }
                );
                (crate::VERSION.to_string(), false)
            }
            None => {
                println!(
                    "could not reach the latest {} release\n\
                     Retry later, or use `rpi update --build --force` for an explicit Cargo build.",
                    if channel == UpdateChannel::Preview {
                        "preview"
                    } else {
                        "stable"
                    }
                );
                return 1;
            }
        };
        // A staged update counts as "already going to happen": it is a fully
        // validated binary waiting only for this process to exit, so compiling
        // a second one buys nothing. It only counts when it is at least as new
        // as the release the registry just reported.
        let staged_wins = staged
            .as_ref()
            .filter(|staged| !is_newer(&staged.to_string(), &latest))
            .map(std::string::ToString::to_string);
        let target = staged_wins.clone().unwrap_or_else(|| latest.clone());
        let needs_update = match channel {
            UpdateChannel::Stable => is_newer(crate::VERSION, &target),
            // Explicitly selecting preview is allowed from a stable install;
            // a preview channel is intentionally independent of the stable channel's
            // stable precedence ordering.
            UpdateChannel::Preview => {
                let current = semver::Version::parse(crate::VERSION).ok();
                current.is_some_and(|current| current.pre.is_empty())
                    || is_newer(crate::VERSION, &target)
            }
        };
        if !needs_update {
            if staged_wins.is_some() {
                println!(
                    "rpi {target} is already staged; quit this rpi and it will be applied\n\
                     Run `rpi update --force` to stage it again."
                );
            } else if verified {
                println!(
                    "rpi is up to date ({} is the latest release)\n\
                     Run `rpi update --force` to reinstall anyway.",
                    crate::VERSION
                );
            } else {
                println!(
                    "rpi is up to date ({}; last known release {latest}, unverified)\n\
                     Run `rpi update --force` to reinstall anyway.",
                    crate::VERSION
                );
            }
            return 0;
        }
    }

    if !build && release.is_none() {
        eprintln!(
            "error: could not resolve the {} release version; retry later or select --build explicitly",
            if channel == UpdateChannel::Preview {
                "preview"
            } else {
                "stable"
            }
        );
        return 1;
    }
    let version = release.as_ref().map(|(version, _)| version.as_str());
    let code = {
        #[cfg(windows)]
        {
            run_windows_self_update(build, version, cdn_base.as_ref())
        }
        #[cfg(not(windows))]
        {
            run_direct_self_update(build, version, cdn_base.as_ref())
        }
    };
    if code == 0 {
        clear_cached_latest_rpi();
    }
    code
}

fn pending_staged_version(channel: UpdateChannel) -> Option<semver::Version> {
    let agent_dir = crate::config::agent_dir().ok()?;
    let update_root = agent_dir.join("self-update");
    staged_pending_version(&update_root, channel)
}

fn cached_latest_rpi() -> Option<String> {
    let agent_dir = crate::config::agent_dir().ok()?;
    let cache = read_cache(&agent_dir.join(CACHE_FILE))?;
    cache.rpi_latest
}

/// A self-update just changed the binary on disk, so the cached release is the
/// version being replaced. Dropping it stops the next startup (and the next
/// `rpi update`) from announcing an update that is already installed; the
/// following registry check refills it.
fn clear_cached_latest_rpi() {
    let Ok(agent_dir) = crate::config::agent_dir() else {
        return;
    };
    let path = agent_dir.join(CACHE_FILE);
    let _guard = UPDATE_CACHE_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut cache) = read_cache(&path) else {
        return;
    };
    cache.rpi_latest = None;
    cache.freshness.rpi = 0;
    let _ = write_cache(&path, &cache);
}

fn cargo_install_command(
    staging_root: Option<&Path>,
    isolated_cwd: &Path,
) -> Result<std::process::Command, String> {
    let isolated_cwd = validated_self_update_command_dir(isolated_cwd)?;
    let mut command = std::process::Command::new("cargo");
    command.arg("install").current_dir(isolated_cwd);
    for variable in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(variable);
    }
    if let Some(staging_root) = staging_root {
        command.arg("--root").arg(staging_root);
    }
    // `--quiet` suppresses cargo's trailing advice to add the install root to
    // PATH. Here that advice is actively wrong: the staging root is a temp
    // directory that the atomic replace consumes, so a user who follows it
    // adds a path that will not exist a second later. Errors still print.
    command.args(["rpi-cli", "--locked", "--force", "--quiet"]);
    Ok(command)
}

/// Both distribution paths feed the same validated staging/atomic replacement.
/// Cargo is only reached when the user explicitly selects --build.
fn prepare_self_update(
    staging: &Path,
    build: bool,
    version: Option<&str>,
    cdn_base: Option<&reqwest::Url>,
) -> Result<(), String> {
    if build {
        let command_dir = create_self_update_command_dir()?;
        let status = cargo_install_command(Some(staging), command_dir.path())?
            .status()
            .map_err(|e| format!("could not run Cargo: {e}"))?;
        return if status.success() {
            Ok(())
        } else {
            Err(format!("cargo install exited with {status}"))
        };
    }
    let version = version
        .ok_or("no verified release version is available")?
        .to_string();
    let staging = staging.to_path_buf();
    let cdn_base = cdn_base.cloned();
    std::thread::Builder::new()
        .name("rpi-prebuilt-update".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(crate::prebuilt_release::download_to(
                &version,
                &staging,
                cdn_base.as_ref(),
            ))
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "prebuilt update worker failed".to_string())?
}

pub(crate) fn validate_downloaded_rpi(
    path: &Path,
    staging: &Path,
    expected: &str,
) -> Result<(), String> {
    let found = validate_rpi_executable_version(path, staging, crate::VERSION, "downloaded rpi")?;
    if found.to_string() != expected {
        return Err(format!(
            "downloaded rpi version {found} does not match manifest {expected}"
        ));
    }
    Ok(())
}

/// Cargo discovers `.cargo/config.toml` from its working directory upward.
/// Put update commands directly below the user's home directory so neither an
/// untrusted project, a project-local `CARGO_HOME`, nor a world-writable system
/// temp directory can join that discovery chain. Cargo still reads its
/// explicitly configured user-level home through its normal environment.
fn create_self_update_command_dir() -> Result<tempfile::TempDir, String> {
    let configured_root =
        dirs::home_dir().ok_or_else(|| "could not resolve the user home directory".to_string())?;
    if !configured_root.is_absolute() {
        return Err("user home must be absolute for self-update".to_string());
    }
    let canonical_root = std::fs::canonicalize(&configured_root)
        .map(normalize_windows_path)
        .map_err(|error| format!("could not canonicalize user home: {error}"))?;
    let metadata = std::fs::symlink_metadata(&canonical_root)
        .map_err(|error| format!("could not inspect user home: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("user home does not resolve to a real directory".to_string());
    }
    let current = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map(normalize_windows_path)
        .map_err(|error| format!("could not validate the caller directory: {error}"))?;
    let project_root = current
        .ancestors()
        .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
        .map(Path::to_path_buf);
    validate_configured_cargo_home(project_root.as_deref())?;
    if project_root.as_deref().is_some_and(|root| {
        !windows_paths_equal(&canonical_root, root) && path_is_within(&canonical_root, root)
    }) {
        return Err(
            "refusing to create a self-update command directory inside the caller project"
                .to_string(),
        );
    }
    let directory = tempfile::Builder::new()
        .prefix("rpi-self-update-command-")
        .tempdir_in(&canonical_root)
        .map_err(|error| format!("could not allocate Cargo command directory: {error}"))?;
    validated_self_update_command_dir(directory.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory_metadata = std::fs::symlink_metadata(directory.path())
            .map_err(|error| format!("could not inspect Cargo command directory: {error}"))?;
        if metadata.uid() != directory_metadata.uid() || metadata.permissions().mode() & 0o022 != 0
        {
            return Err("user home is not private to the current account".to_string());
        }
    }
    Ok(directory)
}

fn path_is_within(candidate: &Path, ancestor: &Path) -> bool {
    candidate
        .ancestors()
        .any(|component| windows_paths_equal(component, ancestor))
}

fn validate_configured_cargo_home(project_root: Option<&Path>) -> Result<(), String> {
    let Some(configured) = std::env::var_os("CARGO_HOME") else {
        return Ok(());
    };
    let configured = PathBuf::from(configured);
    if !configured.is_absolute() {
        return Err("CARGO_HOME must be absolute for self-update".to_string());
    }
    let project_local = match project_root {
        Some(root) => cargo_home_is_project_local(&configured, root)?,
        None => {
            canonicalize_allow_missing(&configured, "CARGO_HOME")?;
            false
        }
    };
    if project_local {
        return Err("refusing project-local CARGO_HOME during self-update".to_string());
    }
    Ok(())
}

fn cargo_home_is_project_local(configured: &Path, project_root: &Path) -> Result<bool, String> {
    if !configured.is_absolute() {
        return Err("CARGO_HOME must be absolute for self-update".to_string());
    }
    let configured = canonicalize_allow_missing(configured, "CARGO_HOME")?;
    Ok(path_is_within(&configured, project_root))
}

fn canonicalize_allow_missing(path: &Path, label: &str) -> Result<PathBuf, String> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(format!("{label} contains relative path components"));
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| format!("could not resolve {label}"))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| format!("could not resolve {label}"))?;
            }
            Err(error) => return Err(format!("could not inspect {label}: {error}")),
        }
    }
    let mut canonical = std::fs::canonicalize(existing)
        .map(normalize_windows_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn canonical_display(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(normalize_windows_path)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unresolvable>".to_string())
}

fn validated_self_update_command_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("self-update command directory must be absolute".to_string());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect self-update command directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("self-update command directory is not a real directory".to_string());
    }
    let canonical = std::fs::canonicalize(path)
        .map(normalize_windows_path)
        .map_err(|error| {
            format!("could not canonicalize self-update command directory: {error}")
        })?;
    Ok(canonical)
}

#[cfg(not(windows))]
#[derive(Debug, Clone)]
struct DirectUpdatePlan {
    staging_dir: PathBuf,
    staged_exe: PathBuf,
    target_exe: PathBuf,
    replacement_temp: PathBuf,
    backup_file: PathBuf,
}

#[cfg(not(windows))]
fn run_direct_self_update(
    build: bool,
    version: Option<&str>,
    cdn_base: Option<&reqwest::Url>,
) -> i32 {
    let staging = match create_self_update_staging_directory() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("error: could not create self-update staging directory: {error}");
            return 1;
        }
    };
    if let Err(error) = prepare_self_update(staging.path(), build, version, cdn_base) {
        eprintln!("error: {error}");
        return 1;
    }

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve the current rpi executable: {error}");
            return 1;
        }
    };
    let plan = match build_direct_update_plan(staging.path(), &current_exe) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    match activate_direct_update(&plan) {
        Ok(version) => {
            println!("rpi updated successfully to {version}");
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn create_self_update_staging_directory() -> Result<tempfile::TempDir, String> {
    let agent = crate::config::agent_dir().map_err(|error| error.to_string())?;
    if !agent.is_absolute() {
        return Err("agent directory must be absolute".to_string());
    }
    std::fs::create_dir_all(&agent)
        .map_err(|error| format!("could not create {}: {error}", agent.display()))?;
    let agent = validated_real_directory(&agent, "agent directory")?;
    let update_root = agent.join("self-update");
    std::fs::create_dir_all(&update_root)
        .map_err(|error| format!("could not create {}: {error}", update_root.display()))?;
    let update_root = validated_real_directory(&update_root, "self-update root")?;
    let staging = tempfile::Builder::new()
        .prefix("pending-")
        .tempdir_in(&update_root)
        .map_err(|error| format!("could not allocate staging directory: {error}"))?;
    validated_real_directory(staging.path(), "self-update staging directory")?;
    Ok(staging)
}

#[cfg(not(windows))]
fn build_direct_update_plan(
    staging_dir: &Path,
    target_exe: &Path,
) -> Result<DirectUpdatePlan, String> {
    let staging_dir = validated_real_directory(staging_dir, "self-update staging directory")?;
    let staged_exe = validated_regular_file(
        &staging_dir.join("bin").join(crate::APP_NAME),
        "staged rpi executable",
    )?;
    let target_exe = validated_regular_file(target_exe, "current rpi executable")?;
    let target_dir = validated_real_directory(
        target_exe
            .parent()
            .ok_or_else(|| "current executable has no parent directory".to_string())?,
        "current executable directory",
    )?;
    if target_exe.parent() != Some(target_dir.as_path()) {
        return Err("current executable is not a direct child of its validated directory".into());
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let replacement_temp = target_dir.join(format!(".rpi-update-{token}.new"));
    let backup_file = target_dir.join(format!(".rpi-update-{token}.old"));
    if replacement_temp.exists() || backup_file.exists() {
        return Err("self-update replacement path collision".to_string());
    }
    Ok(DirectUpdatePlan {
        staging_dir,
        staged_exe,
        target_exe,
        replacement_temp,
        backup_file,
    })
}

#[cfg(not(windows))]
fn activate_direct_update(plan: &DirectUpdatePlan) -> Result<semver::Version, String> {
    let staged_version = validate_rpi_executable_version(
        &plan.staged_exe,
        &plan.staging_dir,
        crate::VERSION,
        "staged rpi",
    )?;
    preflight_direct_replacement(plan)?;

    let result = (|| {
        std::fs::copy(&plan.staged_exe, &plan.replacement_temp).map_err(|error| {
            format!("could not copy staged rpi beside the current executable: {error}")
        })?;
        validated_regular_file(&plan.replacement_temp, "replacement rpi executable")?;
        let copied_version = validate_rpi_executable_version(
            &plan.replacement_temp,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "replacement rpi",
        )?;
        if copied_version != staged_version
            || !files_are_identical(&plan.staged_exe, &plan.replacement_temp)?
        {
            return Err(
                "replacement rpi does not match the validated staged executable".to_string(),
            );
        }

        std::fs::hard_link(&plan.target_exe, &plan.backup_file)
            .map_err(|error| format!("could not preserve the current rpi executable: {error}"))?;
        let installed_version = validate_rpi_executable_version(
            &plan.backup_file,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "current rpi",
        )?;
        if staged_version < installed_version {
            return Err(format!(
                "refusing to replace rpi {installed_version} with older version {staged_version}"
            ));
        }
        if !files_are_identical(&plan.target_exe, &plan.backup_file)? {
            return Err("current rpi executable changed while preparing the update".to_string());
        }

        std::fs::rename(&plan.replacement_temp, &plan.target_exe).map_err(|error| {
            format!("could not atomically replace the current rpi executable: {error}")
        })?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = remove_file_if_exists(&plan.replacement_temp);
        let _ = remove_file_if_exists(&plan.backup_file);
        return Err(error);
    }

    let validation = (|| {
        if !files_are_identical(&plan.staged_exe, &plan.target_exe)? {
            return Err("updated rpi executable does not match the staged executable".to_string());
        }
        let updated_version = validate_rpi_executable_version(
            &plan.target_exe,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "updated rpi",
        )?;
        if updated_version != staged_version {
            return Err(format!(
                "updated rpi reported version {updated_version}, expected {staged_version}"
            ));
        }
        Ok(())
    })();

    if let Err(error) = validation {
        return Err(rollback_direct_update(plan, &error));
    }
    let _ = remove_file_if_exists(&plan.backup_file);
    Ok(staged_version)
}

#[cfg(not(windows))]
fn preflight_direct_replacement(plan: &DirectUpdatePlan) -> Result<(), String> {
    for path in [&plan.replacement_temp, &plan.backup_file] {
        if path.exists() {
            return Err(format!(
                "self-update path already exists: {}",
                path.display()
            ));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("cannot write beside the current executable: {error}"))?;
        drop(file);
        std::fs::remove_file(path)
            .map_err(|error| format!("could not remove self-update probe: {error}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn rollback_direct_update(plan: &DirectUpdatePlan, failure: &str) -> String {
    let _ = remove_file_if_exists(&plan.replacement_temp);
    match std::fs::rename(&plan.backup_file, &plan.target_exe) {
        Ok(()) => format!("{failure}; restored the previous rpi executable"),
        Err(rollback) => format!(
            "{failure}; automatic rollback failed ({rollback}); the previous executable remains at {}",
            plan.backup_file.display()
        ),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct WindowsUpdatePlan {
    staging_dir: PathBuf,
    staged_exe: PathBuf,
    target_exe: PathBuf,
    helper_script: PathBuf,
    status_file: PathBuf,
    replacement_temp: PathBuf,
    backup_file: PathBuf,
    rollback_displaced_file: PathBuf,
}

#[cfg(windows)]
struct WindowsUpdateStaging {
    directory: tempfile::TempDir,
    status_file: PathBuf,
}

#[cfg(windows)]
const WINDOWS_UPDATE_HELPER: &str = r#"param(
    [Parameter(Mandatory = $true)][long] $ParentPid,
    [Parameter(Mandatory = $true)][string] $StagingPath,
    [Parameter(Mandatory = $true)][string] $StagedPath,
    [Parameter(Mandatory = $true)][string] $StagedVersion,
    [Parameter(Mandatory = $true)][string] $TargetPath,
    [Parameter(Mandatory = $true)][string] $TempPath,
    [Parameter(Mandatory = $true)][string] $BackupPath,
    [Parameter(Mandatory = $true)][string] $RollbackDisplacedPath,
    [Parameter(Mandatory = $true)][string] $StatusPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-UpdateStatus {
    param([string] $State, [string] $Message)
    $record = [ordered]@{
        state = $State
        message = $Message
        parentPid = $ParentPid
        staging = $StagingPath
        target = $TargetPath
        staged = $StagedPath
        stagedVersion = $StagedVersion
        backup = $BackupPath
        rollbackDisplaced = $RollbackDisplacedPath
        updatedAt = [DateTime]::UtcNow.ToString('o')
    }
    $json = $record | ConvertTo-Json -Compress
    $statusTemp = $StatusPath + '.tmp'
    [IO.File]::WriteAllText($statusTemp, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $statusTemp -Destination $StatusPath -Force
}

function Assert-RegularFile {
    param([string] $Path, [string] $Label)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or $item.Length -le 0 -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label is not a non-empty regular file"
    }
}

function Assert-WindowsExecutable {
    param([string] $Path, [string] $Label)
    Assert-RegularFile -Path $Path -Label $Label
    $stream = [IO.File]::OpenRead($Path)
    try {
        if ($stream.ReadByte() -ne 77 -or $stream.ReadByte() -ne 90) {
            throw "$Label is not a Windows executable"
        }
    } finally {
        $stream.Dispose()
    }
}

function Assert-RealDirectory {
    param([string] $Path, [string] $Label)
    $item = Get-Item -LiteralPath $Path -Force
    if (-not $item.PSIsContainer -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label is not a real directory"
    }
}

function Get-FileSha256 {
    param([string] $Path)
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            return ([BitConverter]::ToString($sha256.ComputeHash($stream))).Replace('-', '')
        } finally {
            $sha256.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Remove-StagingPayload {
    try {
        $root = [IO.Path]::GetFullPath($StagingPath)
        Assert-RealDirectory -Path $root -Label 'self-update staging directory'
        $bin = [IO.Path]::Combine($root, 'bin')
        $helper = [IO.Path]::Combine($root, 'apply-update.ps1')
        foreach ($file in @(
            $StagedPath,
            [IO.Path]::Combine($root, '.crates.toml'),
            [IO.Path]::Combine($root, '.crates2.json'),
            $helper
        )) {
            if ([IO.File]::Exists($file)) {
                [IO.File]::Delete($file)
            }
        }
        if ([IO.Directory]::Exists($bin) -and
            [IO.Directory]::GetFileSystemEntries($bin).Length -eq 0) {
            [IO.Directory]::Delete($bin, $false)
        }
        if ([IO.Directory]::Exists($root) -and
            [IO.Directory]::GetFileSystemEntries($root).Length -eq 0) {
            [IO.Directory]::Delete($root, $false)
        }
    } catch {
        # Cleanup is best-effort; update status and recovery files stay intact.
    }
}

$replaced = $false
try {
    $stagingFull = [IO.Path]::GetFullPath($StagingPath)
    $stagedFull = [IO.Path]::GetFullPath($StagedPath)
    $targetFull = [IO.Path]::GetFullPath($TargetPath)
    $tempFull = [IO.Path]::GetFullPath($TempPath)
    $backupFull = [IO.Path]::GetFullPath($BackupPath)
    $rollbackDisplacedFull = [IO.Path]::GetFullPath($RollbackDisplacedPath)
    $helperFull = [IO.Path]::Combine($stagingFull, 'apply-update.ps1')
    $statusFull = [IO.Path]::GetFullPath($StatusPath)
    $targetDirectory = [IO.Path]::GetDirectoryName($targetFull)
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName($stagedFull)), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($helperFull), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetFullPath($PSScriptRoot), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($statusFull), [IO.Path]::GetDirectoryName($stagingFull)) -or
        [string]::IsNullOrWhiteSpace($targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($tempFull), $targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($backupFull), $targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($rollbackDisplacedFull), $targetDirectory)) {
        throw 'self-update paths are outside their validated staging or target directories'
    }

    Assert-RealDirectory -Path $stagingFull -Label 'self-update staging directory'
    Assert-RealDirectory -Path $targetDirectory -Label 'target directory'
    Assert-WindowsExecutable -Path $stagedFull -Label 'staged executable'
    Assert-WindowsExecutable -Path $targetFull -Label 'current executable'
    $expectedHash = Get-FileSha256 -Path $stagedFull
    $originalTargetHash = Get-FileSha256 -Path $targetFull
    if ([IO.File]::Exists($tempFull) -or [IO.File]::Exists($backupFull) -or
        [IO.File]::Exists($rollbackDisplacedFull)) {
        throw 'replacement, backup, or rollback path already exists'
    }

    Write-UpdateStatus -State 'waiting' -Message 'waiting for the current rpi process to exit'
    if ($ParentPid -lt 1 -or $ParentPid -gt [int]::MaxValue) {
        throw 'parent process id is outside the supported Windows range'
    }
    $parentProcess = $null
    try {
        $candidate = [Diagnostics.Process]::GetProcessById([int]$ParentPid)
        try {
            $candidatePath = [IO.Path]::GetFullPath($candidate.MainModule.FileName)
            if ([StringComparer]::OrdinalIgnoreCase.Equals($candidatePath, $targetFull)) {
                $parentProcess = $candidate
            } else {
                $candidate.Dispose()
            }
        } catch {
            if ($candidate.HasExited) {
                $candidate.Dispose()
            } else {
                throw
            }
        }
    } catch [ArgumentException] {
        # The invoking process exited before the helper acquired its handle.
    }
    if ($null -ne $parentProcess) {
        try {
            if (-not $parentProcess.WaitForExit(600000)) {
                throw 'timed out waiting for the current rpi process to exit'
            }
        } finally {
            $parentProcess.Dispose()
        }
    }

    Assert-RealDirectory -Path $targetDirectory -Label 'target directory'
    Assert-WindowsExecutable -Path $stagedFull -Label 'staged executable'
    Assert-WindowsExecutable -Path $targetFull -Label 'current executable'
    if ((Get-FileSha256 -Path $stagedFull) -ne $expectedHash) {
        throw 'staged executable changed while waiting for rpi to exit'
    }
    if ((Get-FileSha256 -Path $targetFull) -ne $originalTargetHash) {
        throw 'current executable changed while waiting for rpi to exit'
    }
    if ([IO.File]::Exists($tempFull) -or [IO.File]::Exists($backupFull) -or
        [IO.File]::Exists($rollbackDisplacedFull)) {
        throw 'replacement, backup, or rollback path appeared while waiting for rpi to exit'
    }

    [IO.File]::Copy($stagedFull, $tempFull, $false)
    Assert-WindowsExecutable -Path $tempFull -Label 'replacement executable'
    if ((Get-FileSha256 -Path $tempFull) -ne $expectedHash) {
        throw 'replacement executable hash mismatch'
    }

    [IO.File]::Replace($tempFull, $targetFull, $backupFull, $true)
    $replaced = $true
    Assert-WindowsExecutable -Path $targetFull -Label 'updated executable'
    if ((Get-FileSha256 -Path $targetFull) -ne $expectedHash) {
        throw 'updated executable hash mismatch'
    }
} catch {
    $failureMessage = $_.Exception.Message
    $recoveryMessage = ''
    if ($replaced -and [IO.File]::Exists($BackupPath)) {
        try {
            [IO.File]::Copy($BackupPath, $TempPath, $false)
            Assert-WindowsExecutable -Path $TempPath -Label 'rollback executable'
            if ((Get-FileSha256 -Path $TempPath) -ne $originalTargetHash) {
                throw 'rollback executable hash mismatch'
            }
            [IO.File]::Replace($TempPath, $TargetPath, $RollbackDisplacedPath, $true)
            Assert-WindowsExecutable -Path $TargetPath -Label 'restored executable'
            if ((Get-FileSha256 -Path $TargetPath) -ne $originalTargetHash) {
                throw 'restored executable hash mismatch'
            }
            $replaced = $false
            try { [IO.File]::Delete($BackupPath) } catch {}
            try { [IO.File]::Delete($RollbackDisplacedPath) } catch {}
        } catch {
            $rollbackFailure = $_.Exception.Message
            if ([IO.File]::Exists($BackupPath)) {
                $recoveryMessage = "; automatic rollback failed ($rollbackFailure); the original executable remains at $BackupPath"
            } else {
                $recoveryMessage = "; automatic rollback failed ($rollbackFailure) and no verified recovery backup remains"
            }
        }
    }
    if ([IO.File]::Exists($TempPath)) {
        try { Remove-Item -LiteralPath $TempPath -Force } catch {}
    }
    try { Write-UpdateStatus -State 'failed' -Message ($failureMessage + $recoveryMessage) } catch {}
    Remove-StagingPayload
    exit 1
}

try { Write-UpdateStatus -State 'succeeded' -Message 'rpi executable replaced successfully' } catch {}
if ([IO.File]::Exists($BackupPath)) {
    try { Remove-Item -LiteralPath $BackupPath -Force } catch {}
}
Remove-StagingPayload
exit 0
"#;

#[cfg(windows)]
fn run_windows_self_update(
    build: bool,
    version: Option<&str>,
    cdn_base: Option<&reqwest::Url>,
) -> i32 {
    let staging = match create_windows_update_staging() {
        Ok(staging) => staging,
        Err(error) => {
            eprintln!("error: could not create self-update staging directory: {error}");
            return 1;
        }
    };
    let staging_dir = staging.directory.path().to_path_buf();
    let status_file = staging.status_file.clone();
    let _ = write_windows_update_status(
        &status_file,
        "preparing",
        "installing the update into staging",
    );

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let message = format!("could not resolve the current rpi executable: {error}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
    };
    let powershell = match system_powershell_path() {
        Ok(path) => path,
        Err(error) => {
            let _ = write_windows_update_status(&status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };

    if let Err(error) = prepare_self_update(&staging_dir, build, version, cdn_base) {
        let _ = write_windows_update_status(&status_file, "failed", &error);
        eprintln!("error: {error}");
        return 1;
    }

    let plan = match build_windows_update_plan(&staging_dir, &current_exe, &status_file) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = write_windows_update_status(&status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };
    let staged_version = match validate_staged_rpi(&plan) {
        Ok(version) => version,
        Err(error) => {
            let _ = write_windows_update_status(&plan.status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Err(error) = preflight_windows_replacement(&plan) {
        let _ = write_windows_update_status(&plan.status_file, "failed", &error);
        eprintln!("error: {error}");
        return 1;
    }
    if let Err(error) = std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER.as_bytes()) {
        let message = format!("could not write the self-update helper: {error}");
        let _ = write_windows_update_status(&plan.status_file, "failed", &message);
        eprintln!("error: {message}");
        return 1;
    }
    if validated_regular_file(&plan.helper_script, "self-update helper").is_err() {
        let message = "self-update helper did not pass regular-file validation";
        let _ = write_windows_update_status(&plan.status_file, "failed", message);
        eprintln!("error: {message}");
        return 1;
    }
    let _ = write_windows_update_status(
        &plan.status_file,
        "scheduled",
        &format!("validated rpi {staged_version}; waiting to replace rpi after this process exits"),
    );
    let mut command = windows_update_helper_command(
        &powershell,
        &plan,
        std::process::id(),
        &staged_version.to_string(),
    );
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match command.spawn() {
        Ok(_) => {
            let persisted_staging = staging.directory.keep();
            println!(
                "rpi {staged_version} staged; quit this rpi and it will be applied.\n\
                 Status: {}",
                plan.status_file.display()
            );
            debug_assert!(windows_paths_equal(&persisted_staging, &plan.staging_dir));
            0
        }
        Err(error) => {
            let message = format!("could not start the self-update helper: {error}");
            let _ = write_windows_update_status(&plan.status_file, "failed", &message);
            eprintln!("error: {message}");
            1
        }
    }
}

#[cfg(windows)]
fn create_windows_update_staging() -> Result<WindowsUpdateStaging, String> {
    let staging = create_self_update_staging_directory()?;
    let update_root = staging
        .path()
        .parent()
        .ok_or_else(|| "self-update staging directory has no parent".to_string())?;
    let status_file = update_root.join(format!("status-{}.json", uuid::Uuid::new_v4().simple()));
    if status_file.exists() {
        return Err("self-update status path collision".to_string());
    }
    Ok(WindowsUpdateStaging {
        directory: staging,
        status_file,
    })
}

#[cfg(windows)]
fn build_windows_update_plan(
    staging_dir: &Path,
    target_exe: &Path,
    status_file: &Path,
) -> Result<WindowsUpdatePlan, String> {
    let staging_dir = validated_real_directory(staging_dir, "self-update staging directory")?;
    if !status_file.is_absolute() {
        return Err("self-update status path must be absolute".to_string());
    }
    let status_parent = validated_real_directory(
        status_file
            .parent()
            .ok_or_else(|| "self-update status path has no parent".to_string())?,
        "self-update status directory",
    )?;
    if staging_dir.parent() != Some(status_parent.as_path()) {
        return Err("self-update status file is outside the staging parent".to_string());
    }
    let status_name = status_file
        .file_name()
        .ok_or_else(|| "self-update status file has no name".to_string())?;
    if !is_self_update_status_name(status_name) {
        return Err("self-update status file has an invalid name".to_string());
    }
    if status_file.exists() {
        validated_regular_file(status_file, "self-update status file")?;
    }
    let staged_exe = validated_windows_executable(
        &staging_dir.join("bin").join("rpi.exe"),
        "staged rpi executable",
    )?;
    let target_exe = validated_windows_executable(target_exe, "current rpi executable")?;
    let target_dir = validated_real_directory(
        target_exe
            .parent()
            .ok_or_else(|| "current executable has no parent directory".to_string())?,
        "current executable directory",
    )?;
    if target_exe.parent() != Some(target_dir.as_path()) {
        return Err("current executable is not a direct child of its validated directory".into());
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let replacement_temp = target_dir.join(format!(".rpi-update-{token}.new.exe"));
    let backup_file = target_dir.join(format!(".rpi-update-{token}.old.exe"));
    let rollback_displaced_file = target_dir.join(format!(".rpi-update-{token}.rollback-new.exe"));
    if replacement_temp.exists() || backup_file.exists() || rollback_displaced_file.exists() {
        return Err("self-update replacement path collision".to_string());
    }

    Ok(WindowsUpdatePlan {
        staging_dir: staging_dir.clone(),
        staged_exe,
        target_exe,
        helper_script: staging_dir.join("apply-update.ps1"),
        status_file: status_file.to_path_buf(),
        replacement_temp,
        backup_file,
        rollback_displaced_file,
    })
}

#[cfg(windows)]
fn preflight_windows_replacement(plan: &WindowsUpdatePlan) -> Result<(), String> {
    for path in [
        &plan.replacement_temp,
        &plan.backup_file,
        &plan.rollback_displaced_file,
    ] {
        if path.exists() {
            return Err(format!(
                "self-update path already exists: {}",
                path.display()
            ));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("cannot write beside the current executable: {error}"))?;
        drop(file);
        std::fs::remove_file(path)
            .map_err(|error| format!("could not remove self-update probe: {error}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_staged_rpi(plan: &WindowsUpdatePlan) -> Result<semver::Version, String> {
    let cwd = plan
        .helper_script
        .parent()
        .ok_or_else(|| "self-update staging directory is missing".to_string())?;
    validate_rpi_executable_version(&plan.staged_exe, cwd, crate::VERSION, "staged rpi")
}

/// Captured output of a bounded self-update probe.
struct BoundedProcessOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run a short-lived probe under both a wall-clock and an output cap.
///
/// The child runs on its own thread and current-thread runtime so this stays
/// safe when `rpi update` is entered from inside a Tokio runtime. The old
/// implementation reused the npm bridge's process runner, which went away with
/// the Pi compatibility layer.
fn run_bounded_process_blocking(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<BoundedProcessOutput, String> {
    let program = program.to_string();
    let args = args.to_vec();
    let cwd = cwd.to_path_buf();
    std::thread::Builder::new()
        .name("rpi-bounded-version-probe".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("probe runtime failed: {error}"))?;
            runtime.block_on(async move {
                let child = tokio::process::Command::new(&program)
                    .args(&args)
                    .current_dir(&cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|error| format!("spawn failed: {error}"))?;
                let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
                    Ok(result) => result.map_err(|error| format!("wait failed: {error}"))?,
                    // Dropping the future drops the child, which `kill_on_drop`
                    // turns into a kill.
                    Err(_) => return Err(format!("timed out after {timeout:?}")),
                };
                Ok(BoundedProcessOutput {
                    status: output.status,
                    stdout: cap_bytes(output.stdout, max_output_bytes),
                    stderr: cap_bytes(output.stderr, max_output_bytes),
                })
            })
        })
        .map_err(|error| format!("probe thread failed: {error}"))?
        .join()
        .map_err(|_| "probe worker panicked".to_string())?
}

fn cap_bytes(bytes: Vec<u8>, limit: usize) -> Vec<u8> {
    if bytes.len() <= limit {
        bytes
    } else {
        bytes[..limit].to_vec()
    }
}

/// Byte-for-byte comparison, used to prove a copied or replaced rpi binary is
/// the one that was validated.
#[cfg(not(windows))]
fn files_are_identical(left: &Path, right: &Path) -> Result<bool, String> {
    use std::io::{BufReader, Read};

    let left_file = std::fs::File::open(left)
        .map_err(|error| format!("could not open {}: {error}", left.display()))?;
    let right_file = std::fs::File::open(right)
        .map_err(|error| format!("could not open {}: {error}", right.display()))?;
    if left_file
        .metadata()
        .map_err(|error| error.to_string())?
        .len()
        != right_file
            .metadata()
            .map_err(|error| error.to_string())?
            .len()
    {
        return Ok(false);
    }
    let mut left = BufReader::new(left_file);
    let mut right = BufReader::new(right_file);
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_read = left
            .read(&mut left_buffer)
            .map_err(|error| error.to_string())?;
        let right_read = right
            .read(&mut right_buffer)
            .map_err(|error| error.to_string())?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn validate_rpi_executable_version(
    executable: &Path,
    cwd: &Path,
    current: &str,
    label: &str,
) -> Result<semver::Version, String> {
    let program = executable
        .to_str()
        .ok_or_else(|| format!("{label} path cannot be represented as Unicode"))?;
    let args = vec!["--version".to_string()];
    let output = run_bounded_process_blocking(
        program,
        &args,
        cwd,
        STAGED_RPI_CHECK_TIMEOUT,
        MAX_STAGED_RPI_OUTPUT_BYTES,
    )
    .map_err(|error| format!("{label} version check failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("{label} --version exited with {}", output.status));
    }
    validate_staged_rpi_version_output(&output.stdout, &output.stderr, current)
}

fn validate_staged_rpi_version_output(
    stdout: &[u8],
    stderr: &[u8],
    current: &str,
) -> Result<semver::Version, String> {
    if !stderr.is_empty() {
        return Err("staged rpi --version wrote unexpected stderr output".to_string());
    }
    let staged = parse_rpi_version_output(stdout)
        .ok_or_else(|| "staged rpi returned an invalid --version response".to_string())?;
    let current = semver::Version::parse(current)
        .map_err(|_| "the running rpi version is not valid semantic versioning".to_string())?;
    if staged < current {
        return Err(format!(
            "refusing to replace rpi {current} with older version {staged}"
        ));
    }
    Ok(staged)
}

fn parse_rpi_version_output(output: &[u8]) -> Option<semver::Version> {
    let output = std::str::from_utf8(output).ok()?;
    let line = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if line.contains(['\r', '\n']) {
        return None;
    }
    let version = line.strip_prefix(crate::APP_NAME)?.strip_prefix(' ')?;
    if version.is_empty() || version.chars().any(char::is_whitespace) {
        return None;
    }
    semver::Version::parse(version).ok()
}

/// A real directory *by its own entry*: the leaf is not a link, so the update
/// is free to write into it. Links further up are resolved rather than
/// refused — a launch path such as `C:\tools\current\rpi.exe` where `current`
/// is a junction names the real install directory, which is exactly what an
/// update should rewrite.
fn validated_real_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{label} is a link or junction, not a real directory ({} -> {})",
            path.display(),
            canonical_display(path),
        ));
    }
    if !metadata.is_dir() {
        return Err(format!("{label} is not a real directory"));
    }
    std::fs::canonicalize(path)
        .map(normalize_windows_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))
}

/// A non-empty regular file *by its own entry*: a link to a file is refused
/// with both paths named, because replacing the link would leave the real
/// target stale. Ancestor links are resolved, as in
/// [`validated_real_directory`].
fn validated_regular_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{label} is a link or junction, not a non-empty regular file ({} -> {})",
            path.display(),
            canonical_display(path),
        ));
    }
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{label} is not a non-empty regular file"));
    }
    std::fs::canonicalize(path)
        .map(normalize_windows_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))
}

fn is_self_update_status_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(token) = name
        .strip_prefix("status-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `tempfile::Builder::new().prefix("pending-")` mints the rest of the name
/// from `[A-Za-z0-9]`. Requiring that shape keeps the pruning from ever
/// pointing `remove_dir_all` at a directory this tool did not create.
fn is_self_update_staging_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("pending-") else {
        return false;
    };
    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn validated_self_update_status_file(update_root: &Path, path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.parent() != Some(update_root) {
        return Err("self-update status file is outside the update root".to_string());
    }
    if !path.file_name().is_some_and(is_self_update_status_name) {
        return Err("self-update status file has an invalid name".to_string());
    }
    let path = validated_regular_file(path, "self-update status file")?;
    let length = std::fs::metadata(&path)
        .map_err(|error| format!("could not inspect self-update status file: {error}"))?
        .len();
    if length > MAX_SELF_UPDATE_STATUS_BYTES {
        return Err("self-update status file is too large".to_string());
    }
    Ok(path)
}

fn read_self_update_status(path: &Path) -> Option<SelfUpdateStatus> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::with_capacity(
        usize::try_from(MAX_SELF_UPDATE_STATUS_BYTES)
            .unwrap_or_default()
            .min(8 * 1024),
    );
    file.take(MAX_SELF_UPDATE_STATUS_BYTES.saturating_add(1))
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_SELF_UPDATE_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&data).ok()
}

fn sanitized_self_update_status_message(message: &str) -> String {
    let mut sanitized = String::new();
    let mut pending_space = false;
    let mut truncated = false;
    for character in message.trim().chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }
        if sanitized.chars().count() >= MAX_SELF_UPDATE_STATUS_MESSAGE_CHARS {
            truncated = true;
            break;
        }
        if pending_space {
            sanitized.push(' ');
            pending_space = false;
        }
        sanitized.push(character);
    }
    if sanitized.is_empty() {
        return "no diagnostic message was recorded".to_string();
    }
    if truncated {
        sanitized.push_str("...");
    }
    sanitized
}

fn consume_self_update_statuses(agent_dir: &Path) -> Vec<UpdateWarning> {
    let _guard = SELF_UPDATE_STATUS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let agent_dir = match validated_real_directory(agent_dir, "agent directory") {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let update_root = match validated_real_directory(
        &agent_dir.join("self-update"),
        "self-update status directory",
    ) {
        Ok(path) if path.parent() == Some(agent_dir.as_path()) => path,
        _ => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&update_root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let now = now_ms();
    let mut warnings = Vec::new();
    // Every staging directory a live status file still claims.
    let mut claimed = BTreeSet::new();
    // Staging directories a status file proved to be debris: the helper that
    // would have installed from them is gone, so their on-disk age — which can
    // look recent when a run died early — does not have to vouch for them.
    let mut obsolete = BTreeSet::new();
    // Non-terminal records young enough that the writer is probably still
    // alive. Reconciled after the scan, once a terminal record could have
    // claimed the same directory.
    let mut undecided = Vec::new();
    for entry in entries.flatten() {
        if !is_self_update_status_name(&entry.file_name()) {
            continue;
        }
        let path = match validated_self_update_status_file(&update_root, &entry.path()) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let Some(status) = read_self_update_status(&path) else {
            continue;
        };
        if !matches!(status.state.as_str(), "failed" | "succeeded") {
            // A record with no usable timestamp has an unknown age. Leaving it
            // alone is the safe reading: the alternative deletes a status file
            // that a live helper may still be writing.
            let stale = status.updated_at_ms > 0
                && now.saturating_sub(status.updated_at_ms) > STALE_SELF_UPDATE_STATUS_TTL_MS;
            if stale {
                if let Some(staging) = status.staging.as_deref() {
                    obsolete.insert(staging.to_path_buf());
                }
                let _ = remove_file_if_exists(&path);
            } else {
                undecided.push((path, status));
            }
            continue;
        }
        if let Some(staging) = status.staging.as_deref() {
            claimed.insert(staging.to_path_buf());
        }

        if status.state == "succeeded" {
            // Kept as the durable record of what was installed. The path is
            // printed by `rpi update`, so deleting the file on the next start
            // made a finished update look like it had never run. It ages out
            // with the rest of the record set instead.
            let old = status.updated_at_ms > 0
                && now.saturating_sub(status.updated_at_ms) > COMPLETED_SELF_UPDATE_STATUS_TTL_MS;
            if old {
                if let Some(staging) = status.staging.as_deref() {
                    claimed.remove(staging);
                    obsolete.insert(staging.to_path_buf());
                }
                let _ = remove_file_if_exists(&path);
            }
            continue;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // A failure is still actionable even when cleanup is denied. Keep
            // the file so a later startup can retry consumption.
            Err(_) => {}
        }
        warnings.push(UpdateWarning {
            message: format!(
                "The previously scheduled rpi update failed: {}",
                sanitized_self_update_status_message(&status.message)
            ),
            command: "rpi update".to_string(),
        });
    }

    // A non-terminal record whose staging directory a terminal record already
    // owns is a leftover of a completed run, not a live one.
    for (path, status) in undecided {
        let staging = status.staging.clone();
        if staging
            .as_deref()
            .is_some_and(|staging| claimed.contains(staging))
        {
            let _ = remove_file_if_exists(&path);
            continue;
        }
        if let Some(staging) = staging {
            claimed.insert(staging);
        }
    }

    prune_self_update_staging(&update_root, &claimed, &obsolete, now);
    warnings
}

/// Delete staging directories no live status file references.
///
/// A Windows update persists its staging directory (`TempDir::keep`) so the
/// helper can install from it after the parent exits. When the helper dies
/// first — a reboot at the wrong moment, a machine-wide `taskkill` — the
/// directory stays behind forever: nothing else in the update path removes it,
/// and because its status file was still `preparing` the next start never
/// looked at it either. Those are the directories that pile up in
/// `~/.rpi/agent/self-update`.
///
/// A directory a status file already proved dead (`obsolete`) goes as soon as
/// its name validates. Otherwise the evidence has to come from the directory
/// itself: empty debris goes unconditionally, because no update can be
/// mid-install in a directory with nothing in it, while a populated one waits
/// for the TTL. That keeps a staging directory whose record was just consumed —
/// and which a helper may still be reading — from being deleted underneath it.
fn prune_self_update_staging(
    update_root: &Path,
    claimed: &BTreeSet<PathBuf>,
    obsolete: &BTreeSet<PathBuf>,
    now: i64,
) {
    let Ok(entries) = std::fs::read_dir(update_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_self_update_staging_name(name) {
            continue;
        }
        let path = update_root.join(name);
        if claimed.contains(&path) {
            continue;
        }
        if validated_real_directory(&path, "self-update staging directory").is_err() {
            continue;
        }
        let aged_out = directory_age_ms(
            entry.metadata().ok().and_then(|meta| meta.modified().ok()),
            now,
        )
        .is_some_and(|age| age >= STALE_SELF_UPDATE_STATUS_TTL_MS);
        if obsolete.contains(&path) || !staging_holds_payload(&path) || aged_out {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Whether a staging directory still holds something an install could use.
///
/// `cargo install --root <dir>` leaves zero-byte `.crates.toml` / `.crates2.json`
/// bookkeeping files behind even when it produced no binary, and the helper's
/// own cleanup deletes the payload first. So the presence of *a* file proves
/// nothing — a directory whose only contents are empty files is debris that can
/// never be installed from, and waiting out the TTL for it just keeps garbage
/// around. Only a non-empty file does, and the payload lives one level down in
/// `bin/`.
fn staging_holds_payload(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            if metadata.len() > 0 {
                return true;
            }
            continue;
        }
        if metadata.is_dir()
            && std::fs::read_dir(entry.path()).is_ok_and(|nested| {
                nested.flatten().any(|nested| {
                    nested
                        .metadata()
                        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
                })
            })
        {
            return true;
        }
    }
    false
}

fn directory_age_ms(modified: Option<SystemTime>, now: i64) -> Option<i64> {
    let modified = modified?;
    let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(millis).ok().map(|at| now.saturating_sub(at))
}

#[cfg(windows)]
fn validated_windows_executable(path: &Path, label: &str) -> Result<PathBuf, String> {
    use std::io::Read;

    let path = validated_regular_file(path, label)?;
    let mut file =
        std::fs::File::open(&path).map_err(|error| format!("could not open {label}: {error}"))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read {label}: {error}"))?;
    if magic != *b"MZ" {
        return Err(format!("{label} is not a Windows executable"));
    }
    Ok(path)
}

#[cfg(windows)]
fn system_powershell_path() -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut buffer = vec![0u16; 260];
    let system_root = loop {
        let length = unsafe {
            GetWindowsDirectoryW(
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            )
        };
        if length == 0 {
            return Err(format!(
                "could not locate the Windows system directory: {}",
                std::io::Error::last_os_error()
            ));
        }
        let length = usize::try_from(length)
            .map_err(|error| format!("invalid Windows system directory length: {error}"))?;
        if length < buffer.len() {
            buffer.truncate(length);
            break PathBuf::from(OsString::from_wide(&buffer));
        }
        buffer.resize(length.saturating_add(1), 0);
    };
    let system_root = validated_real_directory(&system_root, "Windows system root")?;
    let powershell = validated_windows_executable(
        &system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        "Windows PowerShell",
    )?;
    powershell
        .strip_prefix(&system_root)
        .map_err(|_| "Windows PowerShell is outside SystemRoot".to_string())?;
    Ok(powershell)
}

#[cfg(windows)]
fn windows_update_helper_command(
    powershell: &Path,
    plan: &WindowsUpdatePlan,
    parent_pid: u32,
    staged_version: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new(powershell);
    let powershell_modules = powershell
        .parent()
        .expect("validated PowerShell path has a parent")
        .join("Modules");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&plan.helper_script)
        .arg("-ParentPid")
        .arg(parent_pid.to_string())
        .arg("-StagingPath")
        .arg(&plan.staging_dir)
        .arg("-StagedPath")
        .arg(&plan.staged_exe)
        .arg("-StagedVersion")
        .arg(staged_version)
        .arg("-TargetPath")
        .arg(&plan.target_exe)
        .arg("-TempPath")
        .arg(&plan.replacement_temp)
        .arg("-BackupPath")
        .arg(&plan.backup_file)
        .arg("-RollbackDisplacedPath")
        .arg(&plan.rollback_displaced_file)
        .arg("-StatusPath")
        .arg(&plan.status_file)
        .current_dir(
            plan.status_file
                .parent()
                .expect("validated status path has an update-root parent"),
        )
        .env("PSModulePath", powershell_modules)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(windows)]
fn write_windows_update_status(path: &Path, state: &str, message: &str) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(&serde_json::json!({
        "state": state,
        "message": message,
        "parentPid": std::process::id(),
        "currentVersion": crate::VERSION,
        "updatedAtMs": now_ms(),
    }))
    .map_err(|error| error.to_string())?;
    std::fs::write(path, data)
        .map_err(|error| format!("could not write update status {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn extension_update_notices_unify_commands_and_separate_source_caches() {
        let _guard = crate::config::test_support::env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let agent = tempfile::tempdir().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, agent.path());
        let record: crate::install::InstalledNativePackage = serde_json::from_value(serde_json::json!({
            "name":"demo", "version":"1.0.0", "source":"prebuilt:https://github.com/owner/repo", "artifacts":["custom.dll"],
            "prebuilt":{"repository":"https://github.com/owner/repo","target":"x86_64-pc-windows-msvc",
                "manifest_sha256":"a".repeat(64),"source_commit":"b".repeat(40),"sdk_version":"0.3.23","plugin_abi":4,
                "archive_sha256":"c".repeat(64),"library_sha256":"d".repeat(64),"custom_repository":true}
        })).unwrap();
        std::fs::write(
            agent.path().join("native-packages.json"),
            serde_json::to_vec(&[record.clone()]).unwrap(),
        )
        .unwrap();
        let mut cache = super::UpdateCache::default();
        cache.native_packages.insert("demo".into(), "99.0.0".into()); // old Cargo source cannot supply a prebuilt update
        assert!(super::report_from_cache(&cache).notices.is_empty());
        cache
            .native_packages
            .insert(super::native_cache_key(&record), "2.0.0".into());
        let report = super::report_from_cache(&cache);
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.notices[0].latest, "2.0.0");
        assert_eq!(report.notices[0].command, "rpi extension update demo");
        let mut other = record.clone();
        other.prebuilt.as_mut().unwrap().repository = "https://github.com/another/repo".into();
        assert_ne!(
            super::native_cache_key(&record),
            super::native_cache_key(&other)
        );

        // A successful install updates the registry. A cached remote version
        // must no longer advertise an update after that version is installed.
        let mut updated = record.clone();
        updated.version = "2.0.0".into();
        std::fs::write(
            agent.path().join("native-packages.json"),
            serde_json::to_vec(&[updated]).unwrap(),
        )
        .unwrap();
        assert!(super::report_from_cache(&cache).notices.is_empty());
        other = record.clone();
        other.prebuilt.as_mut().unwrap().custom_repository = false;
        assert_ne!(
            super::native_cache_key(&record),
            super::native_cache_key(&other)
        );
        let mut cargo = record.clone();
        cargo.source = None;
        cargo.prebuilt = None;
        for (package, command) in [
            (other, "rpi extension update demo"),
            (cargo, "rpi extension update demo"),
        ] {
            cache
                .native_packages
                .insert(super::native_cache_key(&package), "2.0.0".into());
            std::fs::write(
                agent.path().join("native-packages.json"),
                serde_json::to_vec(&[package]).unwrap(),
            )
            .unwrap();
            let report = super::report_from_cache(&cache);
            assert_eq!(report.notices.len(), 1);
            assert_eq!(report.notices[0].command, command);
        }
    }

    /// Fixtures may live under an ambient TEMP that is itself a junction
    /// (e.g. `D:\Temp` on some Windows hosts). Ancestor links are resolved by
    /// the path validation rather than refused, so a plain `tempdir()` is a
    /// valid fixture — and using one keeps these tests honest about that.
    fn real_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("rpi-update-test-")
            .tempdir()
            .unwrap()
    }

    /// The path a fixture resolves to once production validation has seen it.
    ///
    /// Every validated call site canonicalizes and normalizes a path before it
    /// stores or compares it, so a test asserting against a production-derived
    /// path has to resolve its own fixture the same way. `real_tempdir()` stays
    /// unresolved on purpose — that is what keeps these tests honest about an
    /// ambient temp root that is itself a link — but on macOS `/var` is a link
    /// to `/private/var`, so the raw fixture path and the resolved production
    /// path are different strings for the same directory. Comparing them
    /// directly fails for a reason that has nothing to do with the code under
    /// test, and does so only on hosts whose temp root is reached through a
    /// link, which is why a Linux-only CI never saw it.
    fn as_production_resolves(path: &Path) -> std::path::PathBuf {
        super::normalize_windows_path(std::fs::canonicalize(path).unwrap())
    }

    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[cfg(unix)]
    use super::{activate_direct_update, build_direct_update_plan};
    #[cfg(windows)]
    use super::{
        build_windows_update_plan, preflight_windows_replacement, system_powershell_path,
        windows_update_helper_command, write_windows_update_status, WINDOWS_UPDATE_HELPER,
    };
    use super::{
        cargo_home_is_project_local, cargo_install_command, collect_bounded,
        create_self_update_command_dir, fetch_crates_url, is_newer, parse_rpi_version_output,
        path_is_within, results_with_individual_cache_fallback, validate_staged_rpi_version_output,
        RegistryLookup, CACHE_FALLBACK_MAX_AGE_MS, UPDATE_CHECK_CONCURRENCY,
    };

    struct RestoreConfigDir(Option<OsString>);

    impl Drop for RestoreConfigDir {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    struct RestoreEnv {
        name: &'static str,
        value: Option<OsString>,
    }

    impl RestoreEnv {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                value: std::env::var_os(name),
            }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.value.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[cfg(windows)]
    fn fake_windows_executable(path: &Path, marker: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = b"MZ".to_vec();
        bytes.extend_from_slice(marker);
        std::fs::write(path, bytes).unwrap();
    }

    #[cfg(unix)]
    fn fake_unix_rpi(path: &Path, version: &str, fail_after_rename_to: Option<&str>) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let failure = fail_after_rename_to.map_or_else(String::new, |name| {
            format!(
                "case \"$0\" in\n  */{name}) echo 'post-replace failure' >&2; exit 1 ;;\nesac\n"
            )
        });
        std::fs::write(
            path,
            format!("#!/bin/sh\n{failure}printf 'rpi {version}\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_test_update_status_at(path: &Path, state: &str, message: &str, updated_at_ms: i64) {
        let mut status = serde_json::json!({
            "state": state,
            "message": message,
        });
        if updated_at_ms > 0 {
            status["updatedAtMs"] = serde_json::json!(updated_at_ms);
        }
        std::fs::write(path, serde_json::to_vec(&status).unwrap()).unwrap();
    }

    fn write_test_update_status(path: &Path, state: &str, message: &str) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "state": state,
                "message": message,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn path_validation_resolves_ancestor_links_but_refuses_a_link_leaf() {
        let temp = real_tempdir();
        // Real targets, plus a link directory that points at one of them.
        let real_dir = temp.path().join("real-dir");
        let real_file = temp.path().join("real-file.exe");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(&real_file, b"payload").unwrap();

        let link_dir = temp.path().join("link-dir");
        let link_file = temp.path().join("link-file.exe");
        #[cfg(unix)]
        let linked = {
            std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
            std::os::unix::fs::symlink(&real_file, &link_file).unwrap();
            true
        };
        #[cfg(windows)]
        let linked = {
            // Junctions need no elevation; file symlinks do, so skip the file
            // half when this process cannot create one.
            let dir_ok = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&link_dir)
                .arg(&real_dir)
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false);
            if dir_ok {
                let file_ok = std::os::windows::fs::symlink_file(&real_file, &link_file).is_ok();
                if !file_ok {
                    eprintln!("skipping the file-link half: no symlink privilege");
                }
            }
            dir_ok
        };
        if !linked {
            eprintln!("skipping: this platform could not create the link fixture");
            return;
        }

        // A file *below* a linked directory resolves: the link is an ancestor,
        // not the thing being written.
        let through_link = link_dir.join("nested").join("leaf.exe");
        std::fs::create_dir_all(through_link.parent().unwrap()).unwrap();
        std::fs::write(&through_link, b"payload").unwrap();
        super::validated_regular_file(&through_link, "file below a linked directory").unwrap();
        assert_eq!(
            std::fs::canonicalize(&link_dir).unwrap(),
            std::fs::canonicalize(&real_dir).unwrap(),
        );

        // A link *leaf* is still refused, and the error names both paths.
        let error = super::validated_real_directory(&link_dir, "linked directory").unwrap_err();
        assert!(error.contains("link or junction"), "{error}");
        assert!(error.contains(&link_dir.display().to_string()), "{error}");

        if link_file.exists() {
            let error = super::validated_regular_file(&link_file, "linked file").unwrap_err();
            assert!(error.contains("link or junction"), "{error}");
            assert!(error.contains(&link_file.display().to_string()), "{error}");
            assert!(error.contains(&real_file.display().to_string()), "{error}");
        }

        // The link leaf check must not be a blanket refusal: the real targets
        // still validate.
        super::validated_real_directory(&real_dir, "real directory").unwrap();
        super::validated_regular_file(&real_file, "real file").unwrap();
    }

    #[test]
    fn cargo_self_update_staging_is_passed_as_a_structured_argument() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging");
        let command_dir = temp.path().join("command");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&command_dir).unwrap();
        let command = cargo_install_command(Some(&staging), &command_dir).unwrap();
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();

        assert_eq!(command.get_program(), "cargo");
        assert_eq!(args.len(), 7);
        assert_eq!(args[0], "install");
        assert_eq!(args[1], "--root");
        assert_eq!(args[2], staging.as_os_str());
        assert_eq!(args[3], "rpi-cli");
        assert_eq!(args[4], "--locked");
        assert_eq!(args[5], "--force");
        // Suppresses cargo's "add this to your PATH" advice, which names the
        // staging root that the atomic replace is about to consume.
        assert_eq!(args[6], "--quiet");
        // The command runs in the resolved directory, not the unresolved
        // fixture path: `cargo_install_command` validates it first.
        assert_eq!(
            command.get_current_dir(),
            Some(as_production_resolves(&command_dir).as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_update_replaces_the_selected_target_and_rejects_downgrades() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi");
        let target = temp.path().join("target/rpi-current");
        fake_unix_rpi(&staged, "9.9.9", None);
        fake_unix_rpi(&target, crate::VERSION, None);
        let expected = std::fs::read(&staged).unwrap();

        let plan = build_direct_update_plan(&staging, &target).unwrap();
        assert_eq!(activate_direct_update(&plan).unwrap().to_string(), "9.9.9");
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());

        fake_unix_rpi(&staged, "0.0.1", None);
        let plan = build_direct_update_plan(&staging, &target).unwrap();
        let error = activate_direct_update(&plan).unwrap_err();
        assert!(error.contains("older version"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn direct_update_rolls_back_when_post_replace_validation_fails() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi");
        let target = temp.path().join("target/rpi-current");
        fake_unix_rpi(&staged, "9.9.9", Some("rpi-current"));
        fake_unix_rpi(&target, crate::VERSION, None);
        let original = std::fs::read(&target).unwrap();

        let plan = build_direct_update_plan(&staging, &target).unwrap();
        let error = activate_direct_update(&plan).unwrap_err();

        assert!(error.contains("restored the previous"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
    }

    #[test]
    fn self_update_arguments_are_parsed_explicitly() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::parse_self_update_args(&args(&["--build", "--force"])),
            super::SelfUpdateArgs::Run {
                force: true,
                build: true,
                channel: super::UpdateChannel::Stable,
                cdn_base: None,
            }
        );
        assert_eq!(
            super::parse_self_update_args(&args(&[])),
            super::SelfUpdateArgs::Run {
                force: false,
                build: false,
                channel: super::UpdateChannel::Stable,
                cdn_base: None,
            }
        );
        for flag in ["--force", "-f"] {
            assert_eq!(
                super::parse_self_update_args(&args(&[flag])),
                super::SelfUpdateArgs::Run {
                    force: true,
                    build: false,
                    channel: super::UpdateChannel::Stable,
                    cdn_base: None,
                }
            );
        }
        assert_eq!(
            super::parse_self_update_args(&args(&["preview"])),
            super::SelfUpdateArgs::Run {
                force: false,
                build: false,
                channel: super::UpdateChannel::Preview,
                cdn_base: None,
            }
        );
        for flag in ["--help", "-h"] {
            assert_eq!(
                super::parse_self_update_args(&args(&[flag])),
                super::SelfUpdateArgs::Help
            );
        }
        // `--help` wins wherever it appears, and unknown flags are a usage
        // error rather than being silently ignored.
        assert_eq!(
            super::parse_self_update_args(&args(&["--force", "--help"])),
            super::SelfUpdateArgs::Help
        );
        assert_eq!(
            super::parse_self_update_args(&args(&["--nope"])),
            super::SelfUpdateArgs::Usage
        );
    }

    #[test]
    fn default_self_update_requires_a_release_and_never_falls_back_to_cargo() {
        let staging = tempfile::tempdir().unwrap();
        let error = super::prepare_self_update(staging.path(), false, None, None).unwrap_err();
        assert!(error.contains("no verified release version"));
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    }

    #[test]
    fn self_update_honors_env_and_early_dispatched_offline_flag() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore = RestoreEnv::capture(crate::args::RPI_OFFLINE_ENV);

        std::env::set_var(crate::args::RPI_OFFLINE_ENV, "tRuE");
        assert_eq!(super::run_self_update(&[]), 0);

        std::env::set_var(crate::args::RPI_OFFLINE_ENV, "0");
        assert_eq!(super::run_self_update(&["--offline".into()]), 0);
        assert_eq!(
            std::env::var(crate::args::RPI_OFFLINE_ENV).as_deref(),
            Ok("1")
        );
    }

    #[tokio::test]
    async fn offline_skips_rpi_startup_checks_before_cache_io() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore_config = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let _restore_offline = RestoreEnv::capture(crate::args::RPI_OFFLINE_ENV);
        let _restore_disabled = RestoreEnv::capture("RPI_DISABLE_UPDATE_CHECK");
        let temp = real_tempdir();
        let agent = temp.path().join("offline-agent");
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
        std::env::set_var(crate::args::RPI_OFFLINE_ENV, "YES");
        std::env::remove_var("RPI_DISABLE_UPDATE_CHECK");

        let report = super::check_rpi_startup().await;

        assert_eq!(report, super::UpdateReport::default());
        assert!(
            !agent.exists(),
            "offline checks must not create the update cache directory"
        );
    }

    #[test]
    fn completed_self_update_statuses_are_consumed_once() {
        let temp = real_tempdir();
        let agent = temp.path().join("agent");
        let update_root = agent.join("self-update");
        std::fs::create_dir_all(&update_root).unwrap();
        let failed = update_root.join("status-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json");
        let succeeded = update_root.join("status-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.json");
        let waiting = update_root.join("status-cccccccccccccccccccccccccccccccc.json");
        let temporary = update_root.join("status-dddddddddddddddddddddddddddddddd.json.tmp");
        let malformed = update_root.join("status-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json");
        let invalid_name = update_root.join("status-not-a-uuid.json");
        write_test_update_status(&failed, "failed", "permission\u{1b}[31m\r\n denied\u{7}");
        write_test_update_status(&succeeded, "succeeded", "done");
        write_test_update_status(&waiting, "waiting", "still running");
        write_test_update_status(&temporary, "failed", "not published");
        std::fs::write(&malformed, b"{not-json").unwrap();
        write_test_update_status(&invalid_name, "failed", "invalid name");

        let warnings = super::consume_self_update_statuses(&agent);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0]
            .message
            .contains("rpi update failed: permission [31m denied"));
        assert!(!warnings[0].message.chars().any(char::is_control));
        assert_eq!(warnings[0].command, "rpi update");
        assert!(!failed.exists());
        // A `succeeded` record is the receipt for the version now installed —
        // `rpi update` prints its path — so it survives consumption instead of
        // being deleted the moment the next start looks at it.
        assert!(succeeded.exists());
        assert!(waiting.exists());
        assert!(temporary.exists());
        assert!(malformed.exists());
        assert!(invalid_name.exists());
        // The failure is reported once: the record is gone, the kept one is not
        // a failure, and the young `waiting` record says nothing.
        assert!(super::consume_self_update_statuses(&agent).is_empty());
        assert!(succeeded.exists());
    }

    #[test]
    fn stale_and_orphaned_self_update_records_are_pruned() {
        let temp = real_tempdir();
        let agent = temp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        // `consume_self_update_statuses` resolves the agent directory before it
        // derives any staging path from it, so the fixture starts from the same
        // resolved path. Otherwise a staging directory this test records in a
        // status file would never compare equal to the one pruning computes,
        // and the pruning under test would silently not run.
        let agent = as_production_resolves(&agent);
        let update_root = agent.join("self-update");
        std::fs::create_dir_all(&update_root).unwrap();
        let name =
            |token: char| update_root.join(format!("status-{}.json", token.to_string().repeat(32)));
        let now = super::now_ms();
        let stale_ms = now - super::STALE_SELF_UPDATE_STATUS_TTL_MS - 60_000;

        // Live run: recent `waiting` record that owns its staging directory.
        let live_status = name('1');
        let live_staging = update_root.join("pending-Live001");
        std::fs::create_dir_all(live_staging.join("bin")).unwrap();
        std::fs::write(live_staging.join("bin").join("rpi.exe"), b"payload").unwrap();
        std::fs::write(
            &live_status,
            serde_json::to_vec(&serde_json::json!({
                "state": "waiting",
                "staging": live_staging,
                "updatedAtMs": now - 1000,
            }))
            .unwrap(),
        )
        .unwrap();

        // Abandoned run: the helper died, so the record aged out and the
        // payload it claims is debris.
        let dead_status = name('2');
        let dead_staging = update_root.join("pending-Dead002");
        std::fs::create_dir_all(dead_staging.join("bin")).unwrap();
        std::fs::write(dead_staging.join("bin").join("rpi.exe"), b"payload").unwrap();
        write_test_update_status_at(&dead_status, "preparing", "installing", stale_ms);
        // Name the staging directory in the stale record, the way the Windows
        // helper does: that is the evidence its payload is debris even though
        // the directory's own timestamp looks recent.
        let mut dead =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&dead_status).unwrap())
                .unwrap();
        dead["staging"] = serde_json::json!(dead_staging);
        std::fs::write(&dead_status, serde_json::to_vec(&dead).unwrap()).unwrap();

        // Orphan: no status file ever named it, and it was created long ago.
        let orphan = update_root.join("pending-Orphan3");
        std::fs::create_dir_all(orphan.join("bin")).unwrap();
        std::fs::write(orphan.join("bin").join("rpi.exe"), b"payload").unwrap();

        // Debris that can never be serving an update: an empty staging
        // directory, even while it is brand new.
        let empty = update_root.join("pending-Empty04");
        std::fs::create_dir(&empty).unwrap();

        // Not ours to touch: the name is not a tempfile label.
        let foreign = update_root.join("pending-not-ours");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("keep"), b"keep").unwrap();

        let warnings = super::consume_self_update_statuses(&agent);

        assert!(warnings.is_empty());
        assert!(live_status.exists(), "a live run's record must survive");
        assert!(live_staging.exists(), "a live run's staging must survive");
        assert!(!dead_status.exists());
        assert!(!dead_staging.exists());
        // Pruning only deletes a populated directory it can prove is old; an
        // untimestamped orphan is left for a later pass rather than guessed at.
        assert!(orphan.exists());
        // An empty directory cannot be serving an update, whatever its age.
        assert!(!empty.exists());
        assert!(foreign.exists());
    }

    #[test]
    fn completed_records_survive_and_supersede_leftovers() {
        let temp = real_tempdir();
        let agent = temp.path().join("agent");
        let update_root = agent.join("self-update");
        std::fs::create_dir_all(&update_root).unwrap();
        let now = super::now_ms();
        let staging = update_root.join("pending-Done001");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("bin").join("rpi.exe"), b"payload").unwrap();

        // A `waiting` leftover naming the same directory as a completed run.
        let leftover = update_root.join(format!("status-{}.json", "3".repeat(32)));
        std::fs::write(
            &leftover,
            serde_json::to_vec(&serde_json::json!({
                "state": "waiting",
                "staging": staging,
                "updatedAtMs": now,
            }))
            .unwrap(),
        )
        .unwrap();

        let recent = update_root.join(format!("status-{}.json", "4".repeat(32)));
        std::fs::write(
            &recent,
            serde_json::to_vec(&serde_json::json!({
                "state": "succeeded",
                "message": "rpi executable replaced successfully",
                "staging": staging,
                "updatedAtMs": now,
            }))
            .unwrap(),
        )
        .unwrap();

        let aged = update_root.join(format!("status-{}.json", "5".repeat(32)));
        std::fs::write(
            &aged,
            serde_json::to_vec(&serde_json::json!({
                "state": "succeeded",
                "message": "rpi executable replaced successfully",
                "updatedAtMs": now - super::COMPLETED_SELF_UPDATE_STATUS_TTL_MS - 60_000,
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(super::consume_self_update_statuses(&agent).is_empty());

        // The receipt for the installed version is kept until it ages out.
        assert!(recent.exists());
        assert!(!aged.exists());
        // The completed record owns the staging directory, so the leftover
        // `waiting` record that names it is debris; the directory itself is
        // still referenced, so it stays.
        assert!(!leftover.exists());
        assert!(staging.exists());
    }

    #[test]
    fn self_update_status_validation_rejects_escape_and_links() {
        let temp = real_tempdir();
        let agent = temp.path().join("agent");
        let update_root = agent.join("self-update");
        let outside_root = temp.path().join("outside");
        std::fs::create_dir_all(&update_root).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let name = "status-ffffffffffffffffffffffffffffffff.json";
        let outside = outside_root.join(name);
        write_test_update_status(&outside, "failed", "outside");

        assert!(super::validated_self_update_status_file(&update_root, &outside).is_err());

        let linked = update_root.join(name);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        #[cfg(windows)]
        let link_created = std::os::windows::fs::symlink_file(&outside, &linked).is_ok();
        #[cfg(unix)]
        let link_created = true;
        if link_created {
            assert!(super::validated_self_update_status_file(&update_root, &linked).is_err());
            assert!(super::consume_self_update_statuses(&agent).is_empty());
            assert!(outside.exists());
        }
    }

    #[test]
    fn cargo_self_update_without_staging_never_inherits_the_callers_cwd() {
        let temp = real_tempdir();
        let command_dir = temp.path().join("isolated");
        std::fs::create_dir_all(&command_dir).unwrap();

        let command = cargo_install_command(None, &command_dir).unwrap();
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();

        assert_eq!(
            args,
            ["install", "rpi-cli", "--locked", "--force", "--quiet"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            command.get_current_dir(),
            Some(as_production_resolves(&command_dir).as_path())
        );
        assert_ne!(
            command.get_current_dir(),
            std::env::current_dir().ok().as_deref()
        );
        for variable in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        ] {
            assert!(command
                .get_envs()
                .any(|(name, value)| name == variable && value.is_none()));
        }
        assert!(cargo_install_command(None, Path::new("relative-command-dir")).is_err());

        let generated = create_self_update_command_dir().unwrap();
        let caller = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(generated.path().is_absolute());
        assert!(!path_is_within(generated.path(), &caller));
    }

    #[test]
    fn project_local_cargo_home_is_rejected_even_when_its_leaf_is_missing() {
        let temp = real_tempdir();
        let project = temp.path().join("project");
        let global = temp.path().join("global-cargo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        let project = std::fs::canonicalize(project).unwrap();

        assert!(cargo_home_is_project_local(&project.join(".cargo-home"), &project).unwrap());
        assert!(cargo_home_is_project_local(&project, &project).unwrap());
        assert!(!cargo_home_is_project_local(&global, &project).unwrap());
        assert!(cargo_home_is_project_local(Path::new("relative"), &project).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_atomically_replaces_a_valid_target() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging 'semi;& with spaces");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target 'semi;& with spaces/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&staged).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();

        let powershell = system_powershell_path().unwrap();
        let mut command =
            windows_update_helper_command(&powershell, &plan, i32::MAX as u32, "9.9.9");
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        for (flag, expected) in [
            ("-StagingPath", &plan.staging_dir),
            ("-StagedPath", &plan.staged_exe),
            ("-TargetPath", &plan.target_exe),
            ("-TempPath", &plan.replacement_temp),
            ("-BackupPath", &plan.backup_file),
            ("-RollbackDisplacedPath", &plan.rollback_displaced_file),
            ("-StatusPath", &plan.status_file),
        ] {
            let index = args.iter().position(|arg| arg == flag).unwrap();
            assert_eq!(args[index + 1], expected.as_os_str());
        }
        let version_index = args.iter().position(|arg| arg == "-StagedVersion").unwrap();
        assert_eq!(args[version_index + 1], "9.9.9");
        assert_eq!(command.get_current_dir(), plan.status_file.parent());
        assert!(command.get_envs().any(|(name, value)| {
            name == "PSModulePath"
                && value == Some(powershell.parent().unwrap().join("Modules").as_os_str())
        }));
        let status = command.status().unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(
            status.success(),
            "helper exited with {status}: {status_text}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "succeeded");
        assert_eq!(status["stagedVersion"], "9.9.9");
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_failure_preserves_the_old_executable() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging with spaces");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target with spaces/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();
        std::fs::remove_file(&plan.staged_exe).unwrap();

        let status = windows_update_helper_command(
            &system_powershell_path().unwrap(),
            &plan,
            i32::MAX as u32,
            "9.9.9",
        )
        .status()
        .unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(!status.success());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            expected,
            "status={status_text}; backup_exists={}",
            plan.backup_file.exists()
        );
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "failed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_rolls_back_after_post_replace_validation_failure() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        let forced_failure = WINDOWS_UPDATE_HELPER.replace(
            "(Get-FileSha256 -Path $targetFull) -ne $expectedHash",
            "$true",
        );
        assert_ne!(forced_failure, WINDOWS_UPDATE_HELPER);
        std::fs::write(&plan.helper_script, forced_failure).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();

        let status = windows_update_helper_command(
            &system_powershell_path().unwrap(),
            &plan,
            i32::MAX as u32,
            "9.9.9",
        )
        .status()
        .unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(!status.success());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            expected,
            "status={status_text}; backup_exists={}",
            plan.backup_file.exists()
        );
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "failed");
        assert!(status["message"]
            .as_str()
            .unwrap()
            .contains("updated executable hash mismatch"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_plan_rejects_a_non_executable_staged_file() {
        let temp = real_tempdir();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target/rpi.exe");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"not-a-pe").unwrap();
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();

        let error = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap_err();

        assert!(error.contains("not a Windows executable"));
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    }

    #[test]
    fn staged_rpi_version_output_is_exact_and_cannot_downgrade() {
        assert_eq!(
            parse_rpi_version_output(b"rpi 1.2.3\r\n").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert_eq!(
            validate_staged_rpi_version_output(b"rpi 1.2.3\n", b"", "1.2.3").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert!(
            validate_staged_rpi_version_output(b"rpi 1.2.2\n", b"", "1.2.3")
                .unwrap_err()
                .contains("older version")
        );

        for output in [
            &b"pi 1.2.3\n"[..],
            &b"rpi v1.2.3\n"[..],
            &b"rpi 1.2.3 extra\n"[..],
            &b"rpi 1.2.3\nsecond line\n"[..],
            &b" rpi 1.2.3\n"[..],
        ] {
            assert!(
                parse_rpi_version_output(output).is_none(),
                "output={output:?}"
            );
        }
        assert!(
            validate_staged_rpi_version_output(b"rpi 1.2.3\n", b"warning", "1.2.3")
                .unwrap_err()
                .contains("stderr")
        );
    }

    #[test]
    fn preview_release_selection_ignores_stable_and_draft_releases() {
        let releases = vec![
            super::GithubRelease {
                tag_name: "v0.9.4-preview.1".into(),
                draft: false,
                prerelease: true,
            },
            super::GithubRelease {
                tag_name: "v0.9.4-preview.3".into(),
                draft: false,
                prerelease: true,
            },
            super::GithubRelease {
                tag_name: "v0.9.4-preview.99".into(),
                draft: true,
                prerelease: true,
            },
            super::GithubRelease {
                tag_name: "v0.9.4".into(),
                draft: false,
                prerelease: false,
            },
        ];
        assert_eq!(
            super::latest_preview_version(releases),
            super::RegistryLookup::Found("0.9.4-preview.3".into())
        );
    }

    #[test]
    fn compares_release_versions_conservatively() {
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("v1.2.3", "1.3.0"));
        assert!(is_newer("1.2.3-beta.1", "1.2.3-beta.2"));
        assert!(is_newer("1.2.3-beta.2", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.3+build.1"));
        assert!(!is_newer("1.2.3+build.1", "1.2.3+build.2"));
        assert!(!is_newer("nightly", "1.0.0"));
    }

    #[test]
    fn failed_item_checks_keep_only_corresponding_cached_values() {
        let cached = BTreeMap::from([
            ("fresh".to_string(), "1.0.0".to_string()),
            ("failed".to_string(), "2.0.0".to_string()),
            ("removed".to_string(), "3.0.0".to_string()),
        ]);
        let results = vec![
            (
                "fresh".to_string(),
                RegistryLookup::Found("1.1.0".to_string()),
            ),
            ("failed".to_string(), RegistryLookup::TransientFailure),
            ("uncached".to_string(), RegistryLookup::TransientFailure),
            ("missing".to_string(), RegistryLookup::Missing),
        ];

        let (merged, checked_at_values) =
            results_with_individual_cache_fallback(results, Some(&cached), None, 100, 100);
        assert_eq!(merged.get("fresh").map(String::as_str), Some("1.1.0"));
        assert_eq!(merged.get("failed").map(String::as_str), Some("2.0.0"));
        assert!(!merged.contains_key("removed"));
        assert!(!merged.contains_key("uncached"));
        assert!(!merged.contains_key("missing"));
        // A live lookup is stamped with the current check; a fallback keeps the
        // timestamp it was cached at, so the fallback window cannot be extended
        // indefinitely by repeated offline starts.
        assert_eq!(checked_at_values.get("fresh").copied(), Some(100));
        assert_eq!(checked_at_values.get("failed").copied(), Some(100));
        assert!(!checked_at_values.contains_key("uncached"));
        assert!(!checked_at_values.contains_key("missing"));
    }

    #[tokio::test]
    async fn package_update_checks_have_a_shared_concurrency_limit() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let checks = (0..(UPDATE_CHECK_CONCURRENCY * 3)).map(|index| {
            let in_flight = Arc::clone(&in_flight);
            let maximum = Arc::clone(&maximum);
            async move {
                let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                index
            }
        });

        let mut results = collect_bounded(checks).await;
        results.sort_unstable();

        assert_eq!(
            results,
            (0..(UPDATE_CHECK_CONCURRENCY * 3)).collect::<Vec<_>>()
        );
        assert_eq!(maximum.load(Ordering::SeqCst), UPDATE_CHECK_CONCURRENCY);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bounded_checks_do_not_head_of_line_block_new_work() {
        let release_first = Arc::new(tokio::sync::Notify::new());
        let checks = (0..(UPDATE_CHECK_CONCURRENCY + 1)).map(|index| {
            let release_first = Arc::clone(&release_first);
            async move {
                if index == 0 {
                    release_first.notified().await;
                } else if index == UPDATE_CHECK_CONCURRENCY {
                    release_first.notify_one();
                }
                index
            }
        });

        let mut results =
            tokio::time::timeout(std::time::Duration::from_secs(1), collect_bounded(checks))
                .await
                .expect("a completed later check must free a concurrency slot");
        results.sort_unstable();
        assert_eq!(results, (0..=UPDATE_CHECK_CONCURRENCY).collect::<Vec<_>>());
    }

    #[test]
    fn fresh_results_keep_individual_timestamps_when_a_peer_uses_fallback() {
        let now = 10 * CACHE_FALLBACK_MAX_AGE_MS;
        let nearly_stale = now - CACHE_FALLBACK_MAX_AGE_MS + 60_000;
        let cached = BTreeMap::from([
            ("fresh".to_string(), "1.0.0".to_string()),
            ("failed".to_string(), "2.0.0".to_string()),
        ]);
        let freshness = BTreeMap::from([
            ("fresh".to_string(), nearly_stale),
            ("failed".to_string(), nearly_stale),
        ]);
        let (values, timestamps) = results_with_individual_cache_fallback(
            vec![
                (
                    "fresh".to_string(),
                    RegistryLookup::Found("1.1.0".to_string()),
                ),
                ("failed".to_string(), RegistryLookup::TransientFailure),
            ],
            Some(&cached),
            Some(&freshness),
            nearly_stale,
            now,
        );

        assert_eq!(timestamps.get("fresh"), Some(&now));
        assert_eq!(timestamps.get("failed"), Some(&nearly_stale));
        let after_old_entry_expires = now + 120_000;
        let (fallback, _) = results_with_individual_cache_fallback(
            vec![
                ("fresh".to_string(), RegistryLookup::TransientFailure),
                ("failed".to_string(), RegistryLookup::TransientFailure),
            ],
            Some(&values),
            Some(&timestamps),
            nearly_stale,
            after_old_entry_expires,
        );
        assert_eq!(fallback.get("fresh").map(String::as_str), Some("1.1.0"));
        assert!(!fallback.contains_key("failed"));
    }

    #[tokio::test]
    async fn crates_rate_limits_and_request_timeouts_are_transient() {
        async fn lookup(status: &str, body: &str) -> RegistryLookup {
            use std::io::{BufRead, BufReader, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let status = status.to_string();
            let body = body.to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                {
                    let mut reader = BufReader::new(&mut stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let client = reqwest::Client::builder().build().unwrap();
            let result = fetch_crates_url(&client, &format!("http://{address}/crate")).await;
            server.join().unwrap();
            result
        }

        assert_eq!(
            lookup("408 Request Timeout", "").await,
            RegistryLookup::TransientFailure
        );
        assert_eq!(
            lookup("429 Too Many Requests", "").await,
            RegistryLookup::TransientFailure
        );
        let malformed = lookup("200 OK", "{malformed-json").await;
        assert_eq!(malformed, RegistryLookup::TransientFailure);
        assert_eq!(
            super::lookup_with_cache_fallback(malformed, Some("9.9.9"), true),
            (Some("9.9.9".to_string()), true)
        );
        assert_eq!(lookup("404 Not Found", "").await, RegistryLookup::Missing);
    }

    /// One-shot HTTP server: answers the first request with `status`/`body`.
    ///
    /// Returns the URL, a stop flag, and the thread handle. The no-crates.io
    /// regression leaves one server unused, so accept must be nonblocking.
    fn serve_once(
        status: String,
        body: String,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<usize>,
    ) {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // The no-crates.io regression leaves one of two servers unused, so the
        // accept must be able to time out on its own: a blocking accept that no
        // client ever reaches would hang the test, and a self-connect cannot
        // unblock `accept` on this platform.
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let thread_hits = hits.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break Some(stream),
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if thread_stop.load(Ordering::SeqCst)
                            || std::time::Instant::now() > deadline
                        {
                            break None;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break None,
                }
            };
            let Some(mut stream) = stream else {
                return thread_hits.load(Ordering::SeqCst);
            };
            // `accept` inherited non-blocking from the listener; the request read
            // below must block, so put the stream back into blocking mode.
            stream.set_nonblocking(false).unwrap();
            thread_hits.fetch_add(1, Ordering::SeqCst);
            {
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}
Content-Length: {}
Connection: close

{body}",
                body.len()
            )
            .unwrap();
            thread_hits.load(Ordering::SeqCst)
        });
        (format!("http://{address}/release"), stop, handle)
    }

    fn stop_and_join(
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: std::thread::JoinHandle<usize>,
        url: &str,
    ) -> usize {
        use std::sync::atomic::Ordering;
        stop.store(true, Ordering::SeqCst);
        let _ = url;
        handle.join().unwrap()
    }

    fn stable_release(tag: &str) -> String {
        serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false}).to_string()
    }

    #[tokio::test]
    async fn cli_stable_queries_github_without_contacting_crates_io() {
        let (crates_url, crates_stop, crates_server) = serve_once(
            "200 OK".into(),
            r#"{"crate":{"max_version":"99.0.0"}}"#.into(),
        );
        let (github_url, github_stop, github_server) =
            serve_once("200 OK".into(), stable_release("v0.11.0"));
        let client = reqwest::Client::new();
        let result = super::fetch_stable_from_github(&client, &github_url).await;
        assert_eq!(stop_and_join(crates_stop, crates_server, &crates_url), 0);
        assert_eq!(stop_and_join(github_stop, github_server, &github_url), 1);
        assert_eq!(result, RegistryLookup::Found("0.11.0".into()));
    }

    #[tokio::test]
    async fn github_stable_rejects_prereleases_drafts_and_invalid_versions() {
        for body in [
            serde_json::json!({"tag_name":"v1.0.0-preview.1","draft":false,"prerelease":true}),
            serde_json::json!({"tag_name":"v1.0.0","draft":true,"prerelease":false}),
            serde_json::json!({"tag_name":"v1.0.0-rc.1","draft":false,"prerelease":false}),
            serde_json::json!({"tag_name":"1.0.0","draft":false,"prerelease":false}),
            serde_json::json!({"tag_name":"vnot-semver","draft":false,"prerelease":false}),
        ] {
            let (url, stop, server) = serve_once("200 OK".into(), body.to_string());
            let result = super::fetch_stable_from_github(&reqwest::Client::new(), &url).await;
            stop_and_join(stop, server, &url);
            assert_eq!(result, RegistryLookup::TransientFailure, "{body}");
        }
    }

    #[tokio::test]
    async fn github_errors_preserve_fresh_cache_and_reject_expired_cache() {
        let _guard = crate::config::test_support::env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let agent = tempfile::tempdir().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, agent.path());
        for (status, body) in [
            ("404 Not Found", ""),
            ("403 Forbidden", "rate limited"),
            ("500 Internal Server Error", ""),
            ("200 OK", "{not-json"),
        ] {
            for fresh in [true, false] {
                let stamp = super::now_ms() - if fresh { 1000 } else { 48 * 60 * 60 * 1000 };
                let cached = super::UpdateCache {
                    checked_at: stamp,
                    rpi_latest: Some("0.10.0".into()),
                    freshness: super::UpdateCacheFreshness {
                        rpi: stamp,
                        native_packages: Default::default(),
                    },
                    ..Default::default()
                };
                std::fs::write(
                    agent.path().join(super::CACHE_FILE),
                    serde_json::to_vec(&cached).unwrap(),
                )
                .unwrap();
                let (url, stop, server) = serve_once(status.into(), body.into());
                let result = super::lookup_latest_rpi_from(&url).await;
                stop_and_join(stop, server, &url);
                assert_eq!(
                    result,
                    fresh.then(|| ("0.10.0".into(), false)),
                    "{status}, fresh={fresh}"
                );
            }
        }
    }

    #[tokio::test]
    async fn github_live_answer_is_verified_even_with_an_expired_cache() {
        let _guard = crate::config::test_support::env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let agent = tempfile::tempdir().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, agent.path());
        let stamp = super::now_ms() - 48 * 60 * 60 * 1000;
        let cached = super::UpdateCache {
            checked_at: stamp,
            rpi_latest: Some("0.10.0".into()),
            freshness: super::UpdateCacheFreshness {
                rpi: stamp,
                native_packages: Default::default(),
            },
            ..Default::default()
        };
        std::fs::write(
            agent.path().join(super::CACHE_FILE),
            serde_json::to_vec(&cached).unwrap(),
        )
        .unwrap();
        let (url, stop, server) = serve_once("200 OK".into(), stable_release("v0.11.0"));
        let result = super::lookup_latest_rpi_from(&url).await;
        assert_eq!(stop_and_join(stop, server, &url), 1);
        assert_eq!(result, Some(("0.11.0".into(), true)));
    }
}
