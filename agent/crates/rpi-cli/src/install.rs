//! `rpi install` — install a prebuilt or explicitly built Rust `cdylib` extension.
//!
//! Cargo's `cargo install` command is intended for binaries and does not copy
//! dynamic-library targets. rpi extensions are cdylibs loaded by
//! `rpi-extensions`. The default async path uses the prebuilt distribution
//! protocol; the explicit source backend creates a tiny temporary Cargo workspace,
//! resolves the requested crate through Cargo, builds the dependency in
//! release mode, and copies its cdylib into the same global directory scanned
//! during normal startup.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::prebuilt_extensions::{asset_paths_conflict, validate_asset_path};

const INSTALLER_MANIFEST: &str = "rpi-extension-installer";
const NATIVE_PACKAGES_FILE: &str = "native-packages.json";
const NATIVE_PACKAGES_LOCK_FILE: &str = "native-packages.lock";
const CDYLIB_EXTENSION_HINT: &str = "hint: the crate must declare `crate-type = [\"cdylib\"]` and export `rpi_plugin_register` (the unified ABI; legacy `rpi_plugin_register_v2` and `rpi_plugin_register_v3` remain supported for migration)";

/// A Rust-native extension installed through `rpi install`.
///
/// `source` is absent for Cargo registry builds, contains the local source
/// path for `--path`, or marks prebuilt packages so older hosts do not rebuild
/// them. `prebuilt` stores distribution provenance. Local development crates
/// are intentionally not eligible for automatic registry updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledNativePackage {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Exact prebuilt distribution provenance. Missing in older Cargo/local records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt: Option<crate::prebuilt_extensions::PrebuiltRecord>,
    /// Dynamic-library file names copied into the extension store. Older
    /// metadata files omit this field; uninstall falls back to crate-name
    /// matching for those records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

impl InstalledNativePackage {
    /// Prebuilt provenance also uses `source`; only local development builds
    /// should be excluded from registry update discovery and commands.
    pub(crate) fn supports_registry_update(&self) -> bool {
        self.source.is_none() || self.prebuilt.is_some()
    }
}

/// Decide before downloads/builds, then recheck under the package mutation lock.
/// `false` means the requested version is already installed; only --force
/// reinstalls it. Build metadata does not change semantic-version precedence.
pub(crate) fn should_install_version(
    name: &str,
    current: Option<&str>,
    requested: &str,
    force: bool,
) -> Result<bool, String> {
    let Some(current) = current.filter(|_| !force) else {
        return Ok(true);
    };
    let parse = |version: &str| {
        semver::Version::parse(version).map_err(|_| {
            format!("cannot compare version `{version}` for `{name}`; use --force to replace it")
        })
    };
    match parse(requested)?.cmp_precedence(&parse(current)?) {
        std::cmp::Ordering::Greater => Ok(true),
        std::cmp::Ordering::Equal => Ok(false),
        std::cmp::Ordering::Less => Err(format!(
            "{name} {current} is installed; use --force to downgrade to {requested}"
        )),
    }
}

pub(crate) fn print_already_installed(name: &str, version: &str) {
    println!("{name} {version} is already installed; use --force to reinstall it");
}

/// The on-disk state of the Rust-native extension registry.
///
/// A malformed or unreadable registry is returned as an error, rather than
/// being conflated with a missing file. Mutation paths use this distinction to
/// avoid replacing damaged metadata with a newly generated registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePackageRegistry {
    /// No registry has been created yet.
    Missing,
    /// A registry was present and passed JSON/schema validation.
    Valid(Vec<InstalledNativePackage>),
}

impl NativePackageRegistry {
    fn into_records(self) -> Vec<InstalledNativePackage> {
        match self {
            Self::Missing => Vec::new(),
            Self::Valid(records) => records,
        }
    }
}

struct NativePackageMutationLock {
    file: Option<std::fs::File>,
    _resource_lease: crate::resource_management::Lease,
}

impl NativePackageMutationLock {
    fn acquire(metadata_path: &Path) -> Result<Self, String> {
        let parent = metadata_path
            .parent()
            .ok_or_else(|| "native package metadata has no parent".to_string())?;
        let resource_lease = crate::resource_management::shared_lease(parent)?;
        Self::acquire_with_lease(metadata_path, resource_lease)
    }

    fn acquire_exclusive(metadata_path: &Path) -> Result<Self, String> {
        let parent = metadata_path
            .parent()
            .ok_or_else(|| "native package metadata has no parent".to_string())?;
        let resource_lease = crate::resource_management::mutation_lease(parent)?;
        Self::acquire_with_lease(metadata_path, resource_lease)
    }

    fn acquire_with_lease(
        metadata_path: &Path,
        resource_lease: crate::resource_management::Lease,
    ) -> Result<Self, String> {
        let parent = metadata_path
            .parent()
            .ok_or_else(|| "native package metadata has no parent".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let lock_path = parent.join(NATIVE_PACKAGES_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                format!(
                    "could not open native package lock {}: {error}",
                    lock_path.display()
                )
            })?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            format!("could not lock native package state (another install may be running): {error}")
        })?;
        Ok(Self {
            file: Some(file),
            _resource_lease: resource_lease,
        })
    }
}

impl Drop for NativePackageMutationLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs2::FileExt::unlock(&file);
        }
    }
}

#[derive(Debug, Clone)]
struct InstallOptions {
    package: String,
    version: Option<String>,
    path: Option<PathBuf>,
    locked: bool,
    force: bool,
    build: bool,
    repository: Option<String>,
    cdn_base: Option<reqwest::Url>,
}

/// Default to the versioned prebuilt distribution protocol. Cargo builds are
/// explicit (--build or --path) and run off the async worker.
pub async fn run_async(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_help();
        return 0;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("error: {error}");
            print_help();
            return 2;
        }
    };
    if let Err(error) = installed_native_packages_strict() {
        eprintln!("error: cannot safely install while native package metadata is invalid: {error}");
        return 1;
    }
    if options.build || options.path.is_some() {
        let args = args.to_vec();
        return match tokio::task::spawn_blocking(move || run(&args)).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: extension build failed: {error}");
                1
            }
        };
    }
    if crate::args::offline_env_enabled() {
        eprintln!("error: prebuilt installation requires network access; use --build for an explicit Cargo build");
        return 1;
    }
    match crate::prebuilt_extensions::install(
        &options.package,
        options.version.as_deref(),
        options.repository.as_deref(),
        options.force,
        options.cdn_base.as_ref(),
    )
    .await
    {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: String,
    crate_types: Vec<String>,
}

/// Run `rpi install ...`. This is intentionally synchronous: it is a short
/// lived package operation and keeping Cargo's build output attached to the
/// user's terminal makes failures actionable.
pub fn run(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_help();
        return 0;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            print_help();
            return 2;
        }
    };

    // Validate before Cargo work or extension-store mutations. A damaged file
    // may contain the only record of installed artifacts, so treating it as
    // an empty registry would make a later write destructive.
    let records = match installed_native_packages_strict() {
        Ok(records) => records,
        Err(error) => {
            eprintln!(
                "error: cannot safely install while native package metadata is invalid: {error}"
            );
            return 1;
        }
    };
    if !options.build && options.path.is_none() {
        eprintln!(
            "error: prebuilt installs use the async dispatcher; Cargo compilation requires --build"
        );
        return 2;
    }

    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => {
            eprintln!("error: could not create a temporary Cargo workspace: {error}");
            return 1;
        }
    };
    let manifest = temp.path().join("Cargo.toml");
    if let Err(error) = write_manifest(&manifest, &options) {
        eprintln!("error: could not prepare Cargo workspace: {error}");
        return 1;
    }

    if let Err(error) = cargo_command("fetch", &manifest, &options, false) {
        eprintln!("error: could not resolve `{}`: {error}", options.package);
        return 1;
    }

    let metadata = match cargo_metadata(&manifest, &options) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("error: could not inspect `{}`: {error}", options.package);
            return 1;
        }
    };
    let package = match metadata
        .packages
        .iter()
        .find(|package| package.name == options.package)
    {
        Some(package) => package,
        None => {
            eprintln!(
                "error: Cargo did not resolve a package named `{}`",
                options.package
            );
            return 1;
        }
    };
    match should_install_version(
        &package.name,
        records
            .iter()
            .find(|p| p.name == package.name)
            .map(|p| p.version.as_str()),
        &package.version,
        options.force,
    ) {
        Ok(true) => {}
        Ok(false) => {
            print_already_installed(&package.name, &package.version);
            return 0;
        }
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    }
    let cdylib_targets: Vec<&CargoTarget> = package
        .targets
        .iter()
        .filter(|target| target.crate_types.iter().any(|kind| kind == "cdylib"))
        .collect();
    if cdylib_targets.is_empty() {
        eprintln!(
            "error: `{}` is not an rpi extension crate; it has no `cdylib` target",
            options.package
        );
        eprintln!("{CDYLIB_EXTENSION_HINT}");
        return 1;
    }

    if let Err(error) = cargo_command("build", &manifest, &options, true) {
        eprintln!("error: failed to build `{}`: {error}", options.package);
        return 1;
    }

    let artifact_dir = temp.path().join("target").join("release");
    let artifacts = match find_artifacts(&artifact_dir, &cdylib_targets) {
        Ok(artifacts) => artifacts,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let agent_dir = match crate::config::agent_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("error: could not resolve the rpi config directory: {error}");
            return 1;
        }
    };
    let destination = agent_dir.join("extensions");
    if let Err(error) = std::fs::create_dir_all(&destination) {
        eprintln!(
            "error: could not create extension directory {}: {error}",
            destination.display()
        );
        return 1;
    }

    let version = metadata
        .packages
        .iter()
        .find(|candidate| candidate.name == options.package)
        .map(|candidate| candidate.version.clone())
        .unwrap_or_else(|| "0.0.0".to_string());
    let record = InstalledNativePackage {
        name: options.package.clone(),
        version,
        source: options
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        prebuilt: None,
        artifacts: Vec::new(),
    };
    let installed = match install_native_package_at(
        &destination,
        &agent_dir.join(NATIVE_PACKAGES_FILE),
        record,
        &artifacts,
        options.force,
    ) {
        Ok(installed) => installed,
        Err(error) => {
            eprintln!("error: could not install `{}`: {error}", options.package);
            return 1;
        }
    };
    for target in installed {
        println!("installed {}", target.display());
    }
    gc_retired_native_artifacts(&agent_dir);
    println!("rpi will load this extension on the next start.");
    0
}

/// Remove a Rust-native extension installed by `rpi install`.
///
/// The install registry is authoritative for new installs. For metadata from
/// older rpi versions, dynamic libraries whose normalized file stem matches
/// the crate name are removed as a compatibility fallback.
pub fn uninstall(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_uninstall_help();
        return 0;
    }
    let name = match parse_uninstall_name(args) {
        Ok(name) => name,
        Err(error) => {
            eprintln!("error: {error}");
            print_uninstall_help();
            return 2;
        }
    };
    let agent = match crate::config::agent_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve the rpi config directory: {error}");
            return 1;
        }
    };
    let extension_dir = agent.join("extensions");
    let metadata_path = agent.join(NATIVE_PACKAGES_FILE);
    let _lock = match NativePackageMutationLock::acquire_exclusive(&metadata_path) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("error: cannot safely uninstall: {error}");
            return 1;
        }
    };
    let records = match read_native_package_registry_at(&metadata_path) {
        Ok(registry) => registry.into_records(),
        Err(error) => {
            eprintln!(
                "error: cannot safely uninstall while native package metadata is invalid: {error}"
            );
            return 1;
        }
    };
    let mut asset_targets = Vec::new();
    for record in records.iter().filter(|record| record.name == name) {
        for asset in recorded_asset_paths(record) {
            match checked_asset_target(&extension_dir, asset) {
                Ok(path) => asset_targets.push(path),
                Err(error) => {
                    eprintln!("error: cannot safely uninstall: {error}");
                    return 1;
                }
            }
        }
    }
    let had_record = records.iter().any(|record| record.name == name);
    let wanted = normalize_name(&name);
    let artifact_names: std::collections::HashSet<String> = records
        .iter()
        .filter(|record| record.name == name)
        .flat_map(|record| record.artifacts.iter().cloned())
        .collect();
    let use_legacy_name_fallback = artifact_names.is_empty();
    let mut artifact_targets = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&extension_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let is_artifact = artifact_names.contains(&file_name)
                || is_versioned_artifact_for_package(&file_name, &wanted)
                || (use_legacy_name_fallback
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(|stem| normalize_name(stem.trim_start_matches("lib")) == wanted)
                        .unwrap_or(false));
            if is_artifact && is_dynamic_library(&path) {
                artifact_targets.push(path);
            }
        }
    }
    if let Err(error) =
        crate::resource_management::legacy_unowned_targets(&agent, &artifact_targets)
    {
        eprintln!("error: {error}");
        return 1;
    }
    let mut remaining: Vec<_> = records
        .into_iter()
        .filter(|record| record.name != name)
        .collect();
    if had_record {
        remaining.sort_by(|left, right| left.name.cmp(&right.name));
    }
    let mut replacements = Vec::new();
    let mut removals = artifact_targets.clone();
    removals.extend(asset_targets.iter().cloned());
    if had_record {
        if remaining.is_empty() {
            removals.push(metadata_path.clone());
        } else {
            let registry = match serde_json::to_vec_pretty(&remaining) {
                Ok(registry) => registry,
                Err(error) => {
                    eprintln!("error: could not serialize native package metadata: {error}");
                    return 1;
                }
            };
            let staged = match stage_native_bytes(&registry, &metadata_path) {
                Ok(staged) => staged,
                Err(error) => {
                    eprintln!("error: could not stage native package metadata: {error}");
                    return 1;
                }
            };
            replacements.push(NativeReplacement {
                staged,
                target: metadata_path.clone(),
            });
        }
    }
    if let Err(error) = activate_native_transaction(&replacements, &removals, &mut |_, _| Ok(())) {
        eprintln!("error: could not uninstall Rust extension {name}: {error}");
        return 1;
    }
    prune_empty_dirs(&extension_dir, &asset_targets);
    for path in artifact_targets.iter().chain(&asset_targets) {
        println!("removed {}", path.display());
    }
    let removed = artifact_targets.len() + asset_targets.len();
    if removed == 0 && !had_record {
        println!("Rust extension is not installed: {name}");
        return 0;
    }
    println!("uninstalled Rust extension {name}");
    0
}

fn parse_uninstall_name(args: &[String]) -> Result<String, String> {
    let mut name = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Err("use `rpi uninstall --help` for usage".into()),
            value if value.starts_with('-') => {
                return Err(format!("unknown uninstall option `{value}`"))
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    return Err("uninstall accepts exactly one crate name".into());
                }
            }
        }
    }
    let name = name.ok_or_else(|| "missing crate name".to_string())?;
    if !valid_package_name(&name) {
        return Err(format!("invalid Cargo package name `{name}`"));
    }
    Ok(name)
}

#[cfg(test)]
fn write_native_packages_at(path: &Path, records: &[InstalledNativePackage]) -> Result<(), String> {
    if records.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| "native package metadata has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let data = serde_json::to_vec_pretty(records).map_err(|error| error.to_string())?;
    std::fs::write(path, data).map_err(|error| error.to_string())
}

/// Read and validate the Rust-native extension registry without hiding errors.
pub fn read_native_package_registry() -> Result<NativePackageRegistry, String> {
    let path = crate::config::agent_dir()
        .map_err(|error| error.to_string())?
        .join(NATIVE_PACKAGES_FILE);
    read_native_package_registry_at(&path)
}

fn read_native_package_registry_at(path: &Path) -> Result<NativePackageRegistry, String> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NativePackageRegistry::Missing)
        }
        Err(error) => {
            return Err(format!("could not read {}: {error}", path.display()));
        }
    };
    serde_json::from_slice(&data)
        .map(NativePackageRegistry::Valid)
        .map_err(|error| format!("invalid JSON or schema in {}: {error}", path.display()))
}

/// Read the registry for best-effort discovery and startup update notices.
///
/// Missing, unreadable, and malformed registries all produce an empty list so
/// startup remains safe. Install and uninstall operations instead use
/// [`read_native_package_registry`] and fail closed on an invalid file.
pub fn installed_native_packages() -> Vec<InstalledNativePackage> {
    installed_native_packages_strict().unwrap_or_default()
}

/// Read the installed package list for a command that may mutate package
/// state. Unlike startup discovery, callers must surface registry errors and
/// abort before touching any package store.
pub(crate) fn installed_native_packages_strict() -> Result<Vec<InstalledNativePackage>, String> {
    read_native_package_registry().map(NativePackageRegistry::into_records)
}

/// Resolve the active global native package libraries without scanning the
/// extension directory. New records name the active files explicitly; older
/// records had no artifact ledger, so they retain the historical crate-name
/// scan as a compatibility fallback.
pub fn active_global_native_package_artifacts() -> Vec<PathBuf> {
    let Ok(agent_dir) = crate::config::agent_dir() else {
        return Vec::new();
    };
    active_global_native_package_artifacts_at(&agent_dir).unwrap_or_default()
}

/// Resolve unmanaged legacy libraries from the global extension directory.
/// Managed versioned artifacts are deliberately omitted: only the active
/// artifact list above may be loaded for an installed native package.
pub fn unregistered_global_native_package_artifacts() -> Vec<PathBuf> {
    let Ok(agent_dir) = crate::config::agent_dir() else {
        return Vec::new();
    };
    unregistered_global_native_package_artifacts_at(&agent_dir).unwrap_or_default()
}

pub(crate) fn active_global_native_package_artifacts_at(
    agent_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let destination = agent_dir.join("extensions");
    let records =
        read_native_package_registry_at(&agent_dir.join(NATIVE_PACKAGES_FILE))?.into_records();
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for record in records {
        if record.artifacts.is_empty() {
            let wanted = normalize_name(&record.name);
            let Ok(entries) = std::fs::read_dir(&destination) else {
                continue;
            };
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!("could not inspect native extension directory: {error}")
                })?;
                let path = entry.path();
                let matches = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| normalize_name(stem.trim_start_matches("lib")) == wanted)
                    .unwrap_or(false);
                if matches && is_dynamic_library(&path) {
                    let identity = transaction_path_identity(&path);
                    if seen.insert(identity) {
                        paths.push(path);
                    }
                }
            }
        } else {
            for artifact in record.artifacts {
                let path = checked_registered_artifact_path(&destination, &artifact)?;
                if seen.insert(transaction_path_identity(&path)) {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn unregistered_global_native_package_artifacts_at(
    agent_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let destination = agent_dir.join("extensions");
    let records =
        read_native_package_registry_at(&agent_dir.join(NATIVE_PACKAGES_FILE))?.into_records();
    let managed = records
        .iter()
        .flat_map(|record| record.artifacts.iter())
        .map(|artifact| native_artifact_identity(artifact))
        .collect::<HashSet<_>>();
    let legacy_names = records
        .iter()
        .filter(|record| record.artifacts.is_empty())
        .map(|record| normalize_name(&record.name))
        .collect::<HashSet<_>>();
    let mut paths = Vec::new();
    let Ok(entries) = std::fs::read_dir(&destination) else {
        return Ok(paths);
    };
    for entry in entries {
        let path = entry
            .map_err(|error| format!("could not inspect native extension directory: {error}"))?
            .path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let legacy_managed = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| legacy_names.contains(&normalize_name(stem.trim_start_matches("lib"))))
            .unwrap_or(false);
        if !is_dynamic_library(&path)
            || managed.contains(&native_artifact_identity(name))
            || is_versioned_artifact_name(name)
            || legacy_managed
        {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
fn record_native_package_at(path: &Path, record: &InstalledNativePackage) -> Result<(), String> {
    let mut records = read_native_package_registry_at(path)?.into_records();
    if let Some(existing) = records.iter_mut().find(|item| item.name == record.name) {
        *existing = record.clone();
    } else {
        records.push(record.clone());
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    write_native_packages_at(path, &records)
}

struct StagedNativeFile {
    path: PathBuf,
}

impl Drop for StagedNativeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct NativeReplacement {
    staged: StagedNativeFile,
    target: PathBuf,
}

struct NativeBackup {
    target: PathBuf,
    backup: PathBuf,
}

/// A sidecar file of a prebuilt package, already verified against its archive
/// index. `path` is the relative store path, such as `runtime/main.mjs`.
pub(crate) struct NativeAssetFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

pub(crate) fn install_native_package_at(
    destination: &Path,
    metadata_path: &Path,
    record: InstalledNativePackage,
    artifacts: &[PathBuf],
    force: bool,
) -> Result<Vec<PathBuf>, String> {
    install_native_package_with_assets_at(destination, metadata_path, record, artifacts, &[], force)
}

/// Install a library and its sidecar assets as one transaction. The returned paths
/// are the library followed by every asset that was activated.
pub(crate) fn install_native_package_with_assets_at(
    destination: &Path,
    metadata_path: &Path,
    record: InstalledNativePackage,
    artifacts: &[PathBuf],
    assets: &[NativeAssetFile],
    force: bool,
) -> Result<Vec<PathBuf>, String> {
    install_native_package_at_with_hook(
        destination,
        metadata_path,
        record,
        artifacts,
        assets,
        force,
        |_, _| Ok(()),
    )
}

fn install_native_package_at_with_hook(
    destination: &Path,
    metadata_path: &Path,
    mut record: InstalledNativePackage,
    artifacts: &[PathBuf],
    assets: &[NativeAssetFile],
    force: bool,
    mut before_activate: impl FnMut(usize, &Path) -> Result<(), String>,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(destination).map_err(|error| {
        format!(
            "could not create extension directory {}: {error}",
            destination.display()
        )
    })?;
    let metadata_parent = metadata_path
        .parent()
        .ok_or_else(|| "native package metadata has no parent".to_string())?;
    std::fs::create_dir_all(metadata_parent).map_err(|error| error.to_string())?;
    let _lock = NativePackageMutationLock::acquire(metadata_path)?;
    let mut records = read_native_package_registry_at(metadata_path)?.into_records();
    let previous = records
        .iter()
        .find(|candidate| candidate.name == record.name)
        .cloned();

    if !should_install_version(
        &record.name,
        previous.as_ref().map(|p| p.version.as_str()),
        &record.version,
        force,
    )? {
        print_already_installed(&record.name, &record.version);
        return Ok(Vec::new());
    }

    let mut source_artifacts = Vec::with_capacity(artifacts.len());
    let mut source_keys = HashSet::new();
    for artifact in artifacts {
        let metadata = std::fs::symlink_metadata(artifact).map_err(|error| {
            format!(
                "could not inspect built artifact {}: {error}",
                artifact.display()
            )
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "built artifact is not a regular file: {}",
                artifact.display()
            ));
        }
        let name = artifact_file_name(artifact)?;
        if !source_keys.insert(native_artifact_identity(&name)) {
            return Err(format!("duplicate built artifact name `{name}`"));
        }
        source_artifacts.push((name, artifact.clone()));
    }
    if source_artifacts.is_empty() {
        return Err("Cargo produced no installable cdylib artifacts".to_string());
    }

    // Upgrades get a fresh, version-bearing filename, so an already loaded
    // DLL/SO/DYLIB is never replaced or unlinked. First installs retain the
    // historical stable name for compatibility; later upgrades switch to the
    // immutable versioned name.
    let mut new_artifacts = Vec::with_capacity(source_artifacts.len());
    let mut new_keys = HashSet::new();
    for (name, source) in source_artifacts {
        let target_name = if previous.is_some() || (force && destination.join(&name).exists()) {
            versioned_artifact_name(&name, &record.version, destination)?
        } else {
            name
        };
        if !new_keys.insert(native_artifact_identity(&target_name)) {
            return Err(format!("duplicate versioned artifact name `{target_name}`"));
        }
        new_artifacts.push((target_name, source));
    }

    for other in records
        .iter()
        .filter(|candidate| candidate.name != record.name)
    {
        for owned in &other.artifacts {
            checked_registered_artifact_path(destination, owned)?;
            if new_keys.contains(&native_artifact_identity(owned)) {
                return Err(format!(
                    "artifact `{owned}` is already owned by installed package `{}`",
                    other.name
                ));
            }
        }
    }

    // Sidecars share the store namespace with every package's artifacts and assets.
    validate_new_assets(&record, assets, &new_artifacts)?;
    for other in records
        .iter()
        .filter(|candidate| candidate.name != record.name)
    {
        for owned in recorded_asset_paths(other) {
            let clash = new_artifacts
                .iter()
                .map(|(name, _)| name.as_str())
                .chain(assets.iter().map(|asset| asset.path.as_str()))
                .find(|new| asset_paths_conflict(new, owned));
            if let Some(new) = clash {
                return Err(format!(
                    "`{new}` conflicts with asset `{owned}` owned by installed package `{}`",
                    other.name
                ));
            }
        }
    }

    let installed_targets = new_artifacts
        .iter()
        .map(|(name, _)| destination.join(name))
        .collect::<Vec<_>>();
    crate::resource_management::legacy_unowned_targets(metadata_parent, &installed_targets)?;
    for target in &installed_targets {
        match std::fs::symlink_metadata(target) {
            Ok(_) => {
                return Err(format!(
                    "versioned extension target {} already exists",
                    target.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect extension target {}: {error}",
                    target.display()
                ))
            }
        }
    }

    let previous_assets: Vec<String> = previous
        .as_ref()
        .map(|package| {
            recorded_asset_paths(package)
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut asset_targets = Vec::with_capacity(assets.len());
    for asset in assets {
        let target = checked_asset_target(destination, &asset.path)?;
        // Same identity rule as the transaction and the stale check. A case-folded name is
        // owned only when it is the recorded file itself (a case-insensitive store), never a
        // different file that merely differs in case.
        let owned = previous_assets.iter().any(|old| {
            native_artifact_identity(old) == native_artifact_identity(&asset.path)
                || (old.eq_ignore_ascii_case(&asset.path)
                    && same_file(&destination.join(old), &target))
        });
        if !force && !owned {
            match std::fs::symlink_metadata(&target) {
                Ok(_) => {
                    return Err(format!(
                        "extension {} already exists; use --force to replace it",
                        target.display()
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "could not inspect extension target {}: {error}",
                        target.display()
                    ))
                }
            }
        }
        asset_targets.push(target);
    }
    let mut stale_asset_targets = Vec::new();
    for old in &previous_assets {
        // On a case-sensitive store a case-only rename is a different file, and leaving
        // the old one would orphan it. Windows folds identities, as the transaction does.
        if !assets
            .iter()
            .any(|asset| native_artifact_identity(&asset.path) == native_artifact_identity(old))
        {
            stale_asset_targets.push(checked_asset_target(destination, old)?);
        }
    }

    record.artifacts = new_artifacts.iter().map(|(name, _)| name.clone()).collect();
    if let Some(existing) = records
        .iter_mut()
        .find(|candidate| candidate.name == record.name)
    {
        *existing = record;
    } else {
        records.push(record);
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));

    let mut replacements = Vec::with_capacity(new_artifacts.len() + assets.len() + 1);
    for ((_, source), target) in new_artifacts.iter().zip(&installed_targets) {
        replacements.push(NativeReplacement {
            staged: stage_native_artifact(source, target)?,
            target: target.clone(),
        });
    }
    // Sidecars are written into a private directory on the store's filesystem, so
    // the transaction can rename each one into place. Nothing reaches the store
    // until every replacement has been staged.
    let stage = if assets.is_empty() {
        None
    } else {
        Some(StagedDirectory::create(destination)?)
    };
    if let Some(stage) = &stage {
        for (asset, target) in assets.iter().zip(&asset_targets) {
            let staged = stage.path.join(&asset.path);
            if let Some(parent) = staged.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            write_staged_asset(&asset.bytes, &staged)?;
            replacements.push(NativeReplacement {
                staged: StagedNativeFile { path: staged },
                target: target.clone(),
            });
        }
    }
    let registry = serde_json::to_vec_pretty(&records).map_err(|error| error.to_string())?;
    replacements.push(NativeReplacement {
        staged: stage_native_bytes(&registry, metadata_path)?,
        target: metadata_path.to_path_buf(),
    });
    // Directories this transaction creates, so a failed install can remove them again.
    let mut created_dirs = Vec::new();
    for target in &asset_targets {
        if let Some(parent) = target.parent() {
            created_dirs.extend(
                parent
                    .ancestors()
                    .take_while(|dir| *dir != destination && !dir.exists())
                    .map(Path::to_path_buf),
            );
            if let Err(error) = std::fs::create_dir_all(parent) {
                remove_created_dirs(&mut created_dirs);
                return Err(format!(
                    "could not create extension directory {}: {error}",
                    parent.display()
                ));
            }
        }
    }

    // Dynamic-library artifacts are intentionally retained across upgrades.
    // Only replaceable sidecar files follow the old cleanup behavior.
    let mut removals = Vec::new();
    removals.extend(stale_asset_targets.iter().cloned());
    if let Err(error) = activate_native_transaction(&replacements, &removals, &mut before_activate)
    {
        remove_created_dirs(&mut created_dirs);
        return Err(error);
    }
    prune_empty_dirs(destination, &stale_asset_targets);
    Ok(installed_targets.into_iter().chain(asset_targets).collect())
}

/// Assets must match the registry ledger exactly, and each must be a valid,
/// non-overlapping store path that no library artifact of this package occupies.
fn validate_new_assets(
    record: &InstalledNativePackage,
    assets: &[NativeAssetFile],
    artifacts: &[(String, PathBuf)],
) -> Result<(), String> {
    let declared: Vec<&str> = assets.iter().map(|asset| asset.path.as_str()).collect();
    if recorded_asset_paths(record) != declared {
        return Err("internal error: sidecar ledger does not match the staged assets".into());
    }
    for (index, asset) in assets.iter().enumerate() {
        validate_asset_path(&asset.path)?;
        if assets[..index]
            .iter()
            .any(|other| asset_paths_conflict(&other.path, &asset.path))
        {
            return Err(format!(
                "duplicate or overlapping asset path `{}`",
                asset.path
            ));
        }
        if artifacts
            .iter()
            .any(|(name, _)| asset_paths_conflict(name, &asset.path))
        {
            return Err(format!(
                "asset `{}` collides with a library artifact",
                asset.path
            ));
        }
    }
    Ok(())
}

fn recorded_asset_paths(package: &InstalledNativePackage) -> Vec<&str> {
    package
        .prebuilt
        .iter()
        .flat_map(|prebuilt| prebuilt.assets.iter().map(|asset| asset.path.as_str()))
        .collect()
}

/// Resolve a declared sidecar under the store without following links. Existing
/// ancestors must be real directories; missing ones are created by the transaction.
fn checked_asset_target(destination: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_asset_path(relative)?;
    let parts: Vec<&str> = relative.split('/').collect();
    let mut current = destination.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "asset parent {} is not a plain directory",
                    current.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "could not inspect asset parent {}: {error}",
                    current.display()
                ))
            }
        }
    }
    Ok(destination.join(relative))
}

/// A private staging directory, removed on every exit path.
struct StagedDirectory {
    path: PathBuf,
}

impl StagedDirectory {
    fn create(destination: &Path) -> Result<Self, String> {
        let path = destination.join(format!(
            ".rpi-assets-stage-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path).map_err(|error| {
            format!(
                "could not create asset staging directory {}: {error}",
                path.display()
            )
        })?;
        Ok(Self { path })
    }
}

impl Drop for StagedDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_staged_asset(bytes: &[u8], path: &Path) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("could not create staged asset {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write staged asset {}: {error}", path.display()))
}

/// Remove directories a failed transaction created, deepest first. A directory that is
/// not empty (for example after a failed rollback) is kept.
fn remove_created_dirs(dirs: &mut [PathBuf]) {
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs.iter() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Remove directories left empty by removed sidecars. The store itself is kept.
fn prune_empty_dirs(root: &Path, removed: &[PathBuf]) {
    for path in removed {
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == root || !dir.starts_with(root) || std::fs::remove_dir(dir).is_err() {
                break;
            }
            current = dir.parent();
        }
    }
}

fn is_versioned_artifact_name(name: &str) -> bool {
    let Some(stem) = Path::new(name).file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some((prefix, token)) = stem.rsplit_once('-') else {
        return false;
    };
    prefix.rsplit_once("-v").is_some()
        && token.len() == 32
        && token.bytes().all(|value| value.is_ascii_hexdigit())
}

fn is_versioned_artifact_for_package(name: &str, normalized_package: &str) -> bool {
    let Some(stem) = Path::new(name).file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some((prefix, _)) = stem.rsplit_once("-v") else {
        return false;
    };
    is_versioned_artifact_name(name)
        && normalize_name(prefix.trim_start_matches("lib")) == normalized_package
}

/// Best-effort cleanup for retired native artifacts. The exclusive resource
/// lease is the proof that no host is currently holding the old libraries; a
/// live host therefore only causes cleanup to be deferred until a later
/// install or explicit maintenance command.
fn gc_retired_native_artifacts(agent_dir: &Path) {
    let metadata_path = agent_dir.join(NATIVE_PACKAGES_FILE);
    let lease = match crate::resource_management::mutation_lease(agent_dir) {
        Ok(lease) => lease,
        Err(_) => return,
    };
    let _lock = match NativePackageMutationLock::acquire_with_lease(&metadata_path, lease) {
        Ok(lock) => lock,
        Err(_) => return,
    };
    let records = match read_native_package_registry_at(&metadata_path) {
        Ok(registry) => registry.into_records(),
        Err(_) => return,
    };
    let active = records
        .iter()
        .flat_map(|record| record.artifacts.iter())
        .map(|artifact| native_artifact_identity(artifact))
        .collect::<HashSet<_>>();
    let destination = agent_dir.join("extensions");
    let Ok(entries) = std::fs::read_dir(&destination) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !is_dynamic_library(&path)
            || !is_versioned_artifact_name(name)
            || active.contains(&native_artifact_identity(name))
        {
            continue;
        }
        if crate::resource_management::legacy_unowned_targets(
            agent_dir,
            std::slice::from_ref(&path),
        )
        .is_err()
        {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) fn versioned_artifact_name(
    original: &str,
    version: &str,
    destination: &Path,
) -> Result<String, String> {
    let path = Path::new(original);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("artifact has no UTF-8 file stem: {original}"))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("artifact has no dynamic-library extension: {original}"))?;
    let version = version
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    let version = if version.is_empty() {
        "unknown"
    } else {
        &version
    };
    for _ in 0..8 {
        let name = format!(
            "{stem}-v{version}-{}.{}",
            uuid::Uuid::new_v4().simple(),
            extension
        );
        let target = destination.join(&name);
        if matches!(std::fs::symlink_metadata(&target), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(name);
        }
    }
    Err(format!(
        "could not allocate a unique versioned artifact for {original}"
    ))
}

fn artifact_file_name(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("artifact has no UTF-8 file name: {}", path.display()))?;
    checked_registered_artifact_path(Path::new("."), name)?;
    Ok(name.to_string())
}

fn checked_registered_artifact_path(destination: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    let is_single_component = relative.components().count() == 1
        && relative.file_name().and_then(|value| value.to_str()) == Some(name);
    if !is_single_component || !is_dynamic_library(relative) {
        return Err(format!("unsafe native package artifact name `{name}`"));
    }
    Ok(destination.join(relative))
}

fn native_artifact_identity(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

/// Whether two paths name the same existing file (one inode). Windows already folds case
/// in `native_artifact_identity`, so only Unix needs the inode check.
#[cfg(unix)]
fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (
        std::fs::symlink_metadata(left),
        std::fs::symlink_metadata(right),
    ) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_: &Path, _: &Path) -> bool {
    false
}

fn transaction_path_identity(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn adjacent_transaction_path(target: &Path, kind: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("transaction target has no parent: {}", target.display()))?;
    let leaf = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    for _ in 0..8 {
        let candidate = parent.join(format!(
            ".{leaf}.rpi-{kind}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect transaction path {}: {error}",
                    candidate.display()
                ))
            }
        }
    }
    Err(format!(
        "could not allocate a transaction path beside {}",
        target.display()
    ))
}

fn stage_native_artifact(source: &Path, target: &Path) -> Result<StagedNativeFile, String> {
    let stage = adjacent_transaction_path(target, "stage")?;
    let result = (|| {
        let source_metadata = std::fs::symlink_metadata(source).map_err(|error| {
            format!(
                "could not inspect built artifact {}: {error}",
                source.display()
            )
        })?;
        let mut input = std::fs::File::open(source).map_err(|error| {
            format!(
                "could not open built artifact {}: {error}",
                source.display()
            )
        })?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)
            .map_err(|error| {
                format!(
                    "could not create staged artifact {}: {error}",
                    stage.display()
                )
            })?;
        std::io::copy(&mut input, &mut output)
            .and_then(|_| output.sync_all())
            .map_err(|error| {
                format!(
                    "could not write staged artifact {} beside {}: {error}",
                    stage.display(),
                    target.display()
                )
            })?;
        drop(output);
        std::fs::set_permissions(&stage, source_metadata.permissions()).map_err(|error| {
            format!(
                "could not preserve permissions on staged artifact {}: {error}",
                stage.display()
            )
        })
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&stage);
        return Err(error);
    }
    Ok(StagedNativeFile { path: stage })
}
fn stage_native_bytes(bytes: &[u8], target: &Path) -> Result<StagedNativeFile, String> {
    let stage = adjacent_transaction_path(target, "stage")?;
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)
            .map_err(|error| {
                format!(
                    "could not create staged registry {}: {error}",
                    stage.display()
                )
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                format!(
                    "could not flush staged registry {}: {error}",
                    stage.display()
                )
            })
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&stage);
        return Err(error);
    }
    Ok(StagedNativeFile { path: stage })
}

fn activate_native_transaction(
    replacements: &[NativeReplacement],
    removals: &[PathBuf],
    before_activate: &mut impl FnMut(usize, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let mut affected = Vec::new();
    let mut seen = HashSet::new();
    for target in replacements
        .iter()
        .map(|replacement| &replacement.target)
        .chain(removals.iter())
    {
        if seen.insert(transaction_path_identity(target)) {
            affected.push(target.clone());
        }
    }

    for target in &affected {
        match std::fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "refusing to replace non-regular native package file {}",
                    target.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect native package file {}: {error}",
                    target.display()
                ))
            }
        }
    }

    let mut backups = Vec::new();
    for target in &affected {
        if !target.exists() {
            continue;
        }
        let backup = match adjacent_transaction_path(target, "backup") {
            Ok(backup) => backup,
            Err(error) => {
                let rollback = rollback_native_transaction(&[], &backups);
                return Err(transaction_error(error, rollback));
            }
        };
        if let Err(error) = std::fs::rename(target, &backup) {
            let rollback = rollback_native_transaction(&[], &backups);
            return Err(transaction_error(
                format!("could not back up {}: {error}", target.display()),
                rollback,
            ));
        }
        backups.push(NativeBackup {
            target: target.clone(),
            backup,
        });
    }

    let mut activated = Vec::new();
    for (index, replacement) in replacements.iter().enumerate() {
        let activation = before_activate(index, &replacement.target).and_then(|()| {
            std::fs::rename(&replacement.staged.path, &replacement.target).map_err(|error| {
                format!(
                    "could not activate replacement {}: {error}",
                    replacement.target.display()
                )
            })
        });
        if let Err(error) = activation {
            let rollback = rollback_native_transaction(&activated, &backups);
            return Err(transaction_error(error, rollback));
        }
        activated.push(replacement.target.clone());
    }

    for backup in backups {
        if let Err(error) = std::fs::remove_file(&backup.backup) {
            eprintln!(
                "warning: native package updated but backup {} could not be removed: {error}",
                backup.backup.display()
            );
        }
    }
    Ok(())
}

fn rollback_native_transaction(
    activated: &[PathBuf],
    backups: &[NativeBackup],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for target in activated.iter().rev() {
        match std::fs::remove_file(target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("could not remove {}: {error}", target.display())),
        }
    }
    for backup in backups.iter().rev() {
        if let Err(error) = std::fs::rename(&backup.backup, &backup.target) {
            errors.push(format!(
                "could not restore {} from {}: {error}",
                backup.target.display(),
                backup.backup.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn transaction_error(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => error,
        Err(rollback) => format!("{error}; rollback also failed: {rollback}"),
    }
}

fn parse_args(args: &[String]) -> Result<InstallOptions, String> {
    let mut package = None;
    let mut version = None;
    let mut path = None;
    let mut locked = false;
    let mut force = false;
    let mut build = false;
    let mut repository = None;
    let mut cdn = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Err(help_requested().to_string()),
            "--offline" => {} // app normalizes this flag before dispatch
            "--locked" => locked = true,
            "--force" | "-f" => force = true,
            "--build" => build = true,
            "--github" => {
                i += 1;
                repository = Some(value(args, i, "--github")?);
            }
            "--cdn" => {
                i += 1;
                cdn = Some(value(args, i, "--cdn")?);
            }
            "--version" | "-V" => {
                i += 1;
                version = Some(value(args, i, "--version")?);
            }
            "--path" => {
                i += 1;
                path = Some(PathBuf::from(value(args, i, "--path")?));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown install option `{value}`"));
            }
            value => {
                if package.replace(value.to_string()).is_some() {
                    return Err("install accepts exactly one crate name".to_string());
                }
            }
        }
        i += 1;
    }
    if path.is_some() && version.is_some() {
        return Err("--path and --version cannot be used together".to_string());
    }
    if repository.is_some() && (build || path.is_some()) {
        return Err("--github cannot be combined with --build or --path".into());
    }
    if locked && !build && path.is_none() {
        return Err("--locked requires --build or --path".into());
    }
    let package = match package {
        Some(package) => package,
        None => {
            let path = path
                .as_deref()
                .ok_or_else(|| "missing crate name (or use --path <directory>)".to_string())?;
            package_name_from_manifest(path)?
        }
    };
    if !valid_package_name(&package) {
        return Err(format!("invalid Cargo package name `{package}`"));
    }
    let cdn_base = if build || path.is_some() {
        None
    } else {
        crate::cdn::resolve_base(cdn.as_deref())?
    };
    Ok(InstallOptions {
        package,
        version,
        path,
        locked,
        force,
        build,
        repository,
        cdn_base,
    })
}

fn value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn package_name_from_manifest(path: &Path) -> Result<String, String> {
    let manifest = if path.is_dir() {
        path.join("Cargo.toml")
    } else {
        path.to_path_buf()
    };
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .map_err(|error| format!("could not run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not inspect {}: {}",
            manifest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata returned invalid JSON: {error}"))?;
    let manifest = std::fs::canonicalize(&manifest)
        .map_err(|error| format!("could not resolve {}: {error}", manifest.display()))?;
    metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .and_then(|packages| {
            packages.iter().find_map(|package| {
                let package_manifest = package.get("manifest_path")?.as_str()?;
                let package_manifest = std::fs::canonicalize(package_manifest).ok()?;
                (package_manifest == manifest)
                    .then(|| package.get("name")?.as_str().map(str::to_owned))
                    .flatten()
            })
        })
        .filter(|name| valid_package_name(name))
        .ok_or_else(|| format!("{} has no valid [package].name", manifest.display()))
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn help_requested() -> &'static str {
    "use `rpi install --help` for usage"
}

pub fn print_help() {
    println!("Usage: rpi install [<crate>] [options]\n\nInstall or upgrade a prebuilt Rust extension using the versioned rpi-extension.json protocol.\nRepository and latest stable version are resolved from crates.io metadata.\nNewer versions upgrade automatically; an already installed version is skipped.\n\nOptions:\n  --version <version>  Install an exact package version\n  --github <owner/repo> Use this GitHub repository; default version comes from package-specific tags\n  --cdn <URL>           Download release archives from this HTTPS CDN; manifests remain on GitHub\n  --build              Explicitly compile the extension with Cargo\n  --path <directory>   Build a local extension crate; crate name may be omitted\n  --locked             Require Cargo.lock to remain unchanged (with --build/--path)\n  --force, -f          Reinstall the same version or allow a downgrade/replacement\n  --help, -h           Show this help\n\nExamples:\n  rpi install rpi-todo\n  rpi install rpi-todo --version 0.2.0\n  rpi install my-extension --github owner/repo --version 1.2.3\n  rpi install rpi-todo --build\n  rpi install --path . --force\n\nA missing or incompatible prebuilt release is an error; Cargo compilation requires --build.");
}

fn print_uninstall_help() {
    println!(
        "Usage: rpi uninstall <crate>\n\nRemove a Rust cdylib extension installed by `rpi install`.\n\nOptions:\n  --help, -h           Show this help\n\nExample:\n  rpi uninstall rpi-extension-example"
    );
}

fn write_manifest(path: &Path, options: &InstallOptions) -> Result<(), String> {
    let source_dir = path
        .parent()
        .ok_or_else(|| "temporary workspace has no parent directory".to_string())?
        .join("src");
    std::fs::create_dir_all(&source_dir).map_err(|error| error.to_string())?;
    // Cargo requires the temporary root package to have a target even though
    // rpi never builds it; the requested extension is built as a dependency.
    std::fs::write(source_dir.join("lib.rs"), "pub fn installer_marker() {}\n")
        .map_err(|error| error.to_string())?;
    let dependency = if let Some(local_path) = &options.path {
        let absolute = if local_path.is_absolute() {
            local_path.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| error.to_string())?
                .join(local_path)
        };
        format!(
            "rpi_extension_dep = {{ package = {:?}, path = {:?} }}",
            options.package,
            absolute.display().to_string()
        )
    } else {
        let version = options.version.as_deref().unwrap_or("*");
        format!(
            "rpi_extension_dep = {{ package = {:?}, version = {:?} }}",
            options.package, version
        )
    };
    let contents = format!(
        "[package]\nname = \"{INSTALLER_MANIFEST}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\n{dependency}\n"
    );
    std::fs::write(path, contents).map_err(|error| error.to_string())
}

fn cargo_command(
    subcommand: &str,
    manifest: &Path,
    options: &InstallOptions,
    build: bool,
) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.arg(subcommand).arg("--manifest-path").arg(manifest);
    if build {
        command
            .arg("--package")
            .arg(&options.package)
            .arg("--release")
            .arg("--target-dir")
            .arg(manifest.parent().unwrap().join("target"));
    }
    if options.locked {
        command.arg("--locked");
    }
    if crate::args::offline_env_enabled() {
        command.arg("--offline");
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {subcommand} exited with {status}"))
    }
}

fn cargo_metadata(manifest: &Path, options: &InstallOptions) -> Result<CargoMetadata, String> {
    let mut command = Command::new("cargo");
    command
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest);
    if options.locked {
        command.arg("--locked");
    }
    if crate::args::offline_env_enabled() {
        command.arg("--offline");
    }
    let output = command
        .output()
        .map_err(|error| format!("could not execute cargo: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid cargo metadata: {error}"))
}

fn find_artifacts(release_dir: &Path, targets: &[&CargoTarget]) -> Result<Vec<PathBuf>, String> {
    let mut artifacts = Vec::new();
    for target in targets {
        let wanted = normalize_name(&target.name);
        let mut matches = Vec::new();
        for dir in [release_dir.to_path_buf(), release_dir.join("deps")] {
            let entries = std::fs::read_dir(&dir).map_err(|error| {
                format!("could not inspect build output {}: {error}", dir.display())
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_dynamic_library(&path) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let normalized = normalize_name(stem.trim_start_matches("lib"));
                if normalized == wanted {
                    matches.push(path);
                }
            }
        }
        matches.sort_by_key(|path| path.components().count());
        let artifact = matches.into_iter().next().ok_or_else(|| {
            format!(
                "Cargo built `{}` but no cdylib artifact was found in {}",
                target.name,
                release_dir.display()
            )
        })?;
        artifacts.push(artifact);
    }
    Ok(artifacts)
}

fn normalize_name(name: &str) -> String {
    name.replace('-', "_").to_ascii_lowercase()
}

pub(crate) fn is_dynamic_library(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("dll" | "so" | "dylib" | "pyd")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn native_record(name: &str) -> InstalledNativePackage {
        InstalledNativePackage {
            name: name.to_string(),
            version: "1.2.3".to_string(),
            source: None,
            prebuilt: None,
            artifacts: vec![format!("{name}.dll")],
        }
    }

    #[test]
    fn install_version_policy_allows_upgrades_and_requires_force_for_downgrades() {
        assert!(should_install_version("demo", None, "1.0.0", false).unwrap());
        for (current, requested) in [
            ("0.1.11", "0.1.12"),
            ("1.0.0-beta.1", "1.0.0"),
            ("1.0.0-beta.1", "1.0.0-beta.2"),
        ] {
            assert!(should_install_version("demo", Some(current), requested, false).unwrap());
            let error =
                should_install_version("demo", Some(requested), current, false).unwrap_err();
            assert!(error.contains("--force to downgrade"), "{error}");
            assert!(should_install_version("demo", Some(requested), current, true).unwrap());
        }
        for requested in ["1.2.3", "1.2.3+new-build"] {
            assert!(!should_install_version("demo", Some("1.2.3"), requested, false).unwrap());
            assert!(should_install_version("demo", Some("1.2.3"), requested, true).unwrap());
        }
        assert!(should_install_version("demo", Some("unknown"), "1.0.0", false).is_err());
        assert!(should_install_version("demo", Some("unknown"), "1.0.0", true).unwrap());
    }

    #[test]
    fn native_install_same_version_is_unchanged_and_force_reinstalls_or_downgrades() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let build = temp.path().join("demo.dll");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("demo.dll"), b"installed").unwrap();
        let record = native_record("demo");
        write_native_packages_at(&metadata, &[record.clone()]).unwrap();
        let original_registry = std::fs::read(&metadata).unwrap();
        std::fs::write(&build, b"rebuilt").unwrap();

        assert!(install_native_package_at(
            &destination,
            &metadata,
            record.clone(),
            &[build.clone()],
            false,
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            std::fs::read(destination.join("demo.dll")).unwrap(),
            b"installed"
        );
        assert_eq!(std::fs::read(&metadata).unwrap(), original_registry);
        let mut older = record.clone();
        older.version = "1.0.0".into();
        let error = install_native_package_at(
            &destination,
            &metadata,
            older.clone(),
            &[build.clone()],
            false,
        )
        .unwrap_err();
        assert!(error.contains("--force to downgrade"));
        assert_eq!(
            std::fs::read(destination.join("demo.dll")).unwrap(),
            b"installed"
        );
        assert_eq!(std::fs::read(&metadata).unwrap(), original_registry);

        for forced in [record, older] {
            install_native_package_at(
                &destination,
                &metadata,
                forced.clone(),
                &[build.clone()],
                true,
            )
            .unwrap();
            let records = read_native_package_registry_at(&metadata)
                .unwrap()
                .into_records();
            assert_eq!(records[0].version, forced.version);
            let active = destination.join(&records[0].artifacts[0]);
            assert_eq!(std::fs::read(active).unwrap(), b"rebuilt");
            assert_eq!(
                std::fs::read(destination.join("demo.dll")).unwrap(),
                b"installed"
            );
        }
        assert_no_native_transaction_files(&destination);
    }

    #[test]
    fn native_upgrade_does_not_overwrite_unowned_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("demo.dll"), b"installed").unwrap();
        std::fs::write(destination.join("unowned.dll"), b"unowned").unwrap();
        write_native_packages_at(&metadata, &[native_record("demo")]).unwrap();
        let original_registry = std::fs::read(&metadata).unwrap();
        let artifact = temp.path().join("build-unowned.dll");
        std::fs::write(&artifact, b"new").unwrap();
        let mut upgraded = native_record("demo");
        upgraded.version = "2.0.0".into();
        let installed =
            install_native_package_at(&destination, &metadata, upgraded, &[artifact], false)
                .unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(
            std::fs::read(destination.join("demo.dll")).unwrap(),
            b"installed"
        );
        assert_eq!(
            std::fs::read(destination.join("unowned.dll")).unwrap(),
            b"unowned"
        );
        assert_ne!(std::fs::read(&metadata).unwrap(), original_registry);
        assert_no_native_transaction_files(&destination);
    }

    struct TempAgent {
        _guard: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TempAgent {
        fn new() -> Self {
            let guard = crate::config::test_support::env_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let temp = tempfile::tempdir().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, temp.path());
            Self {
                _guard: guard,
                temp,
                previous,
            }
        }

        fn metadata_path(&self) -> PathBuf {
            self.temp.path().join(NATIVE_PACKAGES_FILE)
        }
    }

    impl Drop for TempAgent {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    #[test]
    fn parses_registry_package_and_options() {
        let parsed = parse_args(&args(&["my-extension", "--version", "1.2.3", "--force"])).unwrap();
        assert_eq!(parsed.package, "my-extension");
        assert_eq!(parsed.version.as_deref(), Some("1.2.3"));
        assert!(parsed.force);
    }

    #[test]
    fn github_override_and_source_build_are_explicit_options() {
        let custom = parse_args(&args(&[
            "my-extension",
            "--github",
            "publisher/monorepo",
            "--version",
            "1.2.3",
        ]))
        .unwrap();
        assert_eq!(custom.repository.as_deref(), Some("publisher/monorepo"));
        assert_eq!(custom.version.as_deref(), Some("1.2.3"));
        assert!(!custom.build);
        let custom_url = parse_args(&args(&[
            "my-extension",
            "--github",
            "https://github.com/publisher/monorepo",
        ]))
        .unwrap();
        assert!(custom_url.repository.is_some());
        assert!(
            parse_args(&args(&["my-extension", "--build", "--locked"]))
                .unwrap()
                .build
        );
        assert!(parse_args(&args(&["my-extension", "--offline"])).is_ok());
        assert!(parse_args(&args(&[
            "my-extension",
            "--github",
            "owner/repo",
            "--build"
        ]))
        .is_err());
        assert!(parse_args(&args(&[
            "my-extension",
            "--github",
            "owner/repo",
            "--path",
            "."
        ]))
        .is_err());
        assert!(parse_args(&args(&["my-extension", "--locked"])).is_err());
    }

    #[test]
    fn synchronous_source_backend_cannot_silently_compile_a_prebuilt_request() {
        let _agent = TempAgent::new();
        assert_eq!(run(&args(&["demo"])), 2);
        assert_eq!(
            run(&args(&[
                "demo",
                "--github",
                "publisher/repo",
                "--version",
                "1.2.3"
            ])),
            2
        );
    }

    #[test]
    fn uninstall_prebuilt_package_uses_its_recorded_library_name() {
        let agent = TempAgent::new();
        let directory = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("different_library.dll"), b"prebuilt library").unwrap();
        let record: InstalledNativePackage = serde_json::from_value(serde_json::json!({
            "name":"demo", "version":"1.2.3", "source":"prebuilt:https://github.com/owner/repo",
            "artifacts":["different_library.dll"],
            "prebuilt":{"repository":"https://github.com/owner/repo", "target":"x86_64-pc-windows-msvc",
                "manifest_sha256":"a".repeat(64),"source_commit":"b".repeat(40),"sdk_version":"0.3.23","plugin_abi":4,
                "archive_sha256":"c".repeat(64),"library_sha256":"d".repeat(64),"custom_repository":true}
        })).unwrap();
        write_native_packages_at(&agent.metadata_path(), &[record]).unwrap();
        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!directory.join("different_library.dll").exists());
        assert!(installed_native_packages_strict().unwrap().is_empty());
    }

    #[test]
    fn parses_local_package() {
        let parsed = parse_args(&args(&["--path", "../extension", "my-extension"])).unwrap();
        assert_eq!(parsed.path, Some(PathBuf::from("../extension")));
    }

    #[test]
    fn local_path_can_infer_package_name() {
        let parsed = parse_args(&args(&["--path", ".", "--force"])).unwrap();
        assert!(!parsed.package.is_empty());
        assert!(valid_package_name(&parsed.package));
        assert!(parsed.force);
    }

    #[test]
    fn rejects_non_extension_options_and_invalid_names() {
        assert!(parse_args(&args(&["my.extension"])).is_err());
        assert!(parse_args(&args(&["my-extension", "--unknown"])).is_err());
        assert!(parse_args(&args(&["my-extension", "--path", ".", "--version", "1"])).is_err());
    }

    #[test]
    fn cdylib_hint_recommends_unified_and_documents_legacy_compatibility() {
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register"));
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register_v2"));
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register_v3"));
    }

    #[test]
    fn registry_read_distinguishes_missing_and_valid_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);

        assert_eq!(
            read_native_package_registry_at(&path).unwrap(),
            NativePackageRegistry::Missing
        );

        std::fs::write(
            &path,
            br#"[{"name":"demo","version":"1.2.3","source":null}]"#,
        )
        .unwrap();
        assert_eq!(
            read_native_package_registry_at(&path).unwrap(),
            NativePackageRegistry::Valid(vec![InstalledNativePackage {
                name: "demo".to_string(),
                version: "1.2.3".to_string(),
                source: None,
                prebuilt: None,
                artifacts: Vec::new(),
            }])
        );
    }

    #[test]
    fn active_global_artifacts_exclude_retired_versioned_files() {
        let agent = tempfile::tempdir().unwrap();
        let extensions = agent.path().join("extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        let active = "libdemo-v2.0.0-0123456789abcdef0123456789abcdef.dll";
        let retired = "libdemo-v1.0.0-abcdef0123456789abcdef0123456789.dll";
        std::fs::write(extensions.join(active), b"active").unwrap();
        std::fs::write(extensions.join(retired), b"retired").unwrap();
        std::fs::write(extensions.join("legacy.dll"), b"legacy").unwrap();
        write_native_packages_at(
            &agent.path().join(NATIVE_PACKAGES_FILE),
            &[InstalledNativePackage {
                name: "demo".into(),
                version: "2.0.0".into(),
                source: None,
                prebuilt: None,
                artifacts: vec![active.into()],
            }],
        )
        .unwrap();

        assert_eq!(
            active_global_native_package_artifacts_at(agent.path()).unwrap(),
            vec![extensions.join(active)]
        );
        assert_eq!(
            unregistered_global_native_package_artifacts_at(agent.path()).unwrap(),
            vec![extensions.join("legacy.dll")]
        );
    }

    #[test]
    fn registry_read_rejects_corrupt_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        std::fs::write(&path, b"[{").unwrap();

        let error = read_native_package_registry_at(&path).unwrap_err();
        assert!(error.contains("invalid JSON or schema"));
    }

    #[test]
    fn registry_read_rejects_wrong_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        std::fs::write(&path, br#"{"name":"demo","version":"1.2.3"}"#).unwrap();

        let error = read_native_package_registry_at(&path).unwrap_err();
        assert!(error.contains("invalid JSON or schema"));
    }

    #[test]
    fn record_does_not_replace_a_corrupt_registry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();

        assert!(record_native_package_at(&path, &native_record("demo")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn native_install_transaction_preserves_loaded_artifacts_and_publishes_new_ones() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata_path = temp.path().join(NATIVE_PACKAGES_FILE);
        let build = temp.path().join("build");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(destination.join("old.dll"), b"old-only").unwrap();
        std::fs::write(destination.join("same.dll"), b"old-shared").unwrap();
        let old = InstalledNativePackage {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: None,
            prebuilt: None,
            artifacts: vec!["old.dll".into(), "same.dll".into()],
        };
        write_native_packages_at(&metadata_path, &[old]).unwrap();
        std::fs::write(build.join("new.dll"), b"new-only").unwrap();
        std::fs::write(build.join("same.dll"), b"new-shared").unwrap();

        let installed = install_native_package_at(
            &destination,
            &metadata_path,
            InstalledNativePackage {
                name: "demo".into(),
                version: "2.0.0".into(),
                source: None,
                prebuilt: None,
                artifacts: Vec::new(),
            },
            &[build.join("new.dll"), build.join("same.dll")],
            false,
        )
        .unwrap();

        assert_eq!(installed.len(), 2);
        assert!(installed.iter().all(|path| path.exists()));
        assert_eq!(
            std::fs::read(destination.join("old.dll")).unwrap(),
            b"old-only"
        );
        assert_eq!(
            std::fs::read(destination.join("same.dll")).unwrap(),
            b"old-shared"
        );
        assert!(installed
            .iter()
            .any(|path| { std::fs::read(path).ok().as_deref() == Some(b"new-only".as_slice()) }));
        assert!(installed
            .iter()
            .any(|path| { std::fs::read(path).ok().as_deref() == Some(b"new-shared".as_slice()) }));
        let records = read_native_package_registry_at(&metadata_path)
            .unwrap()
            .into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].version, "2.0.0");
        let active_names = installed
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(records[0].artifacts, active_names);
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    #[test]
    fn native_install_transaction_rolls_back_artifacts_and_registry() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata_path = temp.path().join(NATIVE_PACKAGES_FILE);
        let build = temp.path().join("build");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(destination.join("old.dll"), b"old-only").unwrap();
        std::fs::write(destination.join("same.dll"), b"old-shared").unwrap();
        let old = InstalledNativePackage {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: None,
            prebuilt: None,
            artifacts: vec!["old.dll".into(), "same.dll".into()],
        };
        write_native_packages_at(&metadata_path, &[old]).unwrap();
        let original_registry = std::fs::read(&metadata_path).unwrap();
        std::fs::write(build.join("new.dll"), b"new-only").unwrap();
        std::fs::write(build.join("same.dll"), b"new-shared").unwrap();

        let error = install_native_package_at_with_hook(
            &destination,
            &metadata_path,
            InstalledNativePackage {
                name: "demo".into(),
                version: "2.0.0".into(),
                source: None,
                prebuilt: None,
                artifacts: Vec::new(),
            },
            &[build.join("new.dll"), build.join("same.dll")],
            &[],
            false,
            |_, target| {
                if target == metadata_path {
                    Err("injected registry activation failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("injected registry activation failure"));
        assert_eq!(std::fs::read(&metadata_path).unwrap(), original_registry);
        assert_eq!(
            std::fs::read(destination.join("old.dll")).unwrap(),
            b"old-only"
        );
        assert_eq!(
            std::fs::read(destination.join("same.dll")).unwrap(),
            b"old-shared"
        );
        assert!(!destination.join("new.dll").exists());
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    fn assert_no_native_transaction_files(directory: &Path) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".rpi-stage-"), "left staged file: {name}");
            assert!(!name.contains(".rpi-backup-"), "left backup file: {name}");
            assert!(
                !name.contains(".rpi-assets-stage-"),
                "left asset stage: {name}"
            );
        }
    }

    #[test]
    fn best_effort_registry_read_omits_corrupt_metadata() {
        let agent = TempAgent::new();
        std::fs::write(agent.metadata_path(), b"[{broken metadata").unwrap();

        assert!(installed_native_packages().is_empty());
    }

    #[test]
    fn install_fails_closed_before_work_when_registry_is_corrupt() {
        let agent = TempAgent::new();
        let path = agent.metadata_path();
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();

        assert_eq!(run(&args(&["demo"])), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!agent.temp.path().join("extensions").exists());
    }

    #[test]
    fn uninstall_fails_closed_without_removing_artifacts() {
        let agent = TempAgent::new();
        let path = agent.metadata_path();
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        let artifact = extension_dir.join("demo.dll");
        std::fs::write(&artifact, b"extension").unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(artifact).unwrap(), b"extension");
    }

    #[test]
    fn uninstall_commits_artifact_and_registry_changes_together() {
        let agent = TempAgent::new();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(extension_dir.join("demo.dll"), b"demo").unwrap();
        std::fs::write(extension_dir.join("other.dll"), b"other").unwrap();
        write_native_packages_at(
            &agent.metadata_path(),
            &[native_record("demo"), native_record("other")],
        )
        .unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!extension_dir.join("demo.dll").exists());
        assert_eq!(
            std::fs::read(extension_dir.join("other.dll")).unwrap(),
            b"other"
        );
        assert_eq!(
            read_native_package_registry_at(&agent.metadata_path())
                .unwrap()
                .into_records(),
            vec![native_record("other")]
        );
        assert_no_native_transaction_files(agent.temp.path());
        assert_no_native_transaction_files(&extension_dir);
    }

    #[test]
    fn uninstall_uses_recorded_artifacts_without_name_fallback() {
        let agent = TempAgent::new();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(extension_dir.join("recorded.dll"), b"managed").unwrap();
        std::fs::write(extension_dir.join("demo.dll"), b"unmanaged").unwrap();
        write_native_packages_at(
            &agent.metadata_path(),
            &[InstalledNativePackage {
                name: "demo".into(),
                version: "1.2.3".into(),
                source: None,
                prebuilt: None,
                artifacts: vec!["recorded.dll".into()],
            }],
        )
        .unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!extension_dir.join("recorded.dll").exists());
        assert_eq!(
            std::fs::read(extension_dir.join("demo.dll")).unwrap(),
            b"unmanaged"
        );
        assert!(!agent.metadata_path().exists());
        assert_no_native_transaction_files(agent.temp.path());
        assert_no_native_transaction_files(&extension_dir);
    }

    /// A prebuilt record whose ledger names `assets`, as the archive installer writes it.
    fn prebuilt_record(
        name: &str,
        version: &str,
        assets: &[(&str, &[u8])],
    ) -> InstalledNativePackage {
        InstalledNativePackage {
            name: name.into(),
            version: version.into(),
            source: Some(format!("prebuilt:https://github.com/owner/{name}")),
            prebuilt: Some(crate::prebuilt_extensions::PrebuiltRecord {
                repository: format!("https://github.com/owner/{name}"),
                target: "x86_64-unknown-linux-gnu".into(),
                manifest_sha256: "a".repeat(64),
                source_commit: "b".repeat(40),
                sdk_version: "0.3.23".into(),
                plugin_abi: 4,
                archive_sha256: "c".repeat(64),
                library_sha256: "d".repeat(64),
                custom_repository: false,
                assets: assets
                    .iter()
                    .map(|(path, bytes)| crate::prebuilt_extensions::PrebuiltAsset {
                        path: (*path).into(),
                        sha256: crate::prebuilt_extensions::hash(bytes),
                    })
                    .collect(),
            }),
            artifacts: Vec::new(),
        }
    }

    fn sidecars(assets: &[(&str, &[u8])]) -> Vec<NativeAssetFile> {
        assets
            .iter()
            .map(|(path, bytes)| NativeAssetFile {
                path: (*path).into(),
                bytes: bytes.to_vec(),
            })
            .collect()
    }

    #[test]
    fn native_asset_upgrade_replaces_sidecars_and_prunes_stale_ones() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"library v1").unwrap();
        let v1 = [
            ("runtime/main.mjs", &b"v1"[..]),
            ("runtime/old.mjs", &b"old"[..]),
        ];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &v1),
            std::slice::from_ref(&library),
            &sidecars(&v1),
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("runtime/old.mjs")).unwrap(),
            b"old"
        );

        std::fs::write(&library, b"library v2").unwrap();
        let v2 = [
            ("runtime/main.mjs", &b"v2"[..]),
            ("runtime/deep/lib.mjs", &b"deep"[..]),
        ];
        let installed = install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "2.0.0", &v2),
            std::slice::from_ref(&library),
            &sidecars(&v2),
            false,
        )
        .unwrap();
        assert_eq!(installed.len(), 3);
        let active_library = installed
            .iter()
            .find(|path| is_dynamic_library(path))
            .expect("installed library");
        assert_ne!(active_library, &destination.join("libdemo.so"));
        assert_eq!(std::fs::read(active_library).unwrap(), b"library v2");
        assert_eq!(
            std::fs::read(destination.join("libdemo.so")).unwrap(),
            b"library v1"
        );
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"v2"
        );
        assert_eq!(
            std::fs::read(destination.join("runtime/deep/lib.mjs")).unwrap(),
            b"deep"
        );
        assert!(!destination.join("runtime/old.mjs").exists());
        let records = read_native_package_registry_at(&metadata)
            .unwrap()
            .into_records();
        assert_eq!(records[0].version, "2.0.0");
        let ledger: Vec<&str> = recorded_asset_paths(&records[0]);
        assert_eq!(ledger, vec!["runtime/main.mjs", "runtime/deep/lib.mjs"]);
        // Dropping every sidecar removes the files and the directories they emptied.
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "3.0.0", &[]),
            std::slice::from_ref(&library),
            &[],
            false,
        )
        .unwrap();
        assert!(!destination.join("runtime").exists());
        let records = read_native_package_registry_at(&metadata)
            .unwrap()
            .into_records();
        let active_library = destination.join(&records[0].artifacts[0]);
        assert_eq!(std::fs::read(active_library).unwrap(), b"library v2");
        assert_eq!(
            std::fs::read(destination.join("libdemo.so")).unwrap(),
            b"library v1"
        );
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    #[test]
    fn native_asset_failure_during_activation_restores_the_previous_install() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"old library").unwrap();
        let v1 = [("runtime/main.mjs", &b"v1"[..])];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &v1),
            std::slice::from_ref(&library),
            &sidecars(&v1),
            false,
        )
        .unwrap();
        let original_registry = std::fs::read(&metadata).unwrap();

        std::fs::write(&library, b"new library").unwrap();
        let v2 = [
            ("runtime/main.mjs", &b"v2"[..]),
            ("runtime/extra.mjs", &b"extra"[..]),
        ];
        // Replacements activate in order: library, main.mjs, extra.mjs, registry.
        let error = install_native_package_at_with_hook(
            &destination,
            &metadata,
            prebuilt_record("demo", "2.0.0", &v2),
            std::slice::from_ref(&library),
            &sidecars(&v2),
            false,
            |index, _| {
                if index == 2 {
                    Err("injected sidecar activation failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(
            error.contains("injected sidecar activation failure"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(destination.join("libdemo.so")).unwrap(),
            b"old library"
        );
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"v1"
        );
        assert!(!destination.join("runtime/extra.mjs").exists());
        assert_eq!(std::fs::read(&metadata).unwrap(), original_registry);
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
        assert_no_native_transaction_files(&destination.join("runtime"));

        // The same upgrade succeeds once the failure is gone.
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "2.0.0", &v2),
            std::slice::from_ref(&library),
            &sidecars(&v2),
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"v2"
        );
        assert_eq!(
            std::fs::read(destination.join("runtime/extra.mjs")).unwrap(),
            b"extra"
        );
    }

    #[test]
    fn native_asset_conflicts_and_overlaps_fail_before_any_write() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"demo library").unwrap();
        let other = temp.path().join("libother.so");
        std::fs::write(&other, b"other library").unwrap();
        let shared = [("runtime/shared.mjs", &b"other"[..])];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("other", "1.0.0", &shared),
            std::slice::from_ref(&other),
            &sidecars(&shared),
            false,
        )
        .unwrap();
        let original_registry = std::fs::read(&metadata).unwrap();

        let attempts: [&[(&str, &[u8])]; 3] = [
            &[("runtime/shared.mjs", &b"demo"[..])],
            &[("Runtime/Shared.mjs", &b"demo"[..])],
            &[("runtime", &b"file"[..]), ("runtime/own.mjs", &b"own"[..])],
        ];
        for assets in attempts {
            let error = install_native_package_with_assets_at(
                &destination,
                &metadata,
                prebuilt_record("demo", "1.0.0", assets),
                std::slice::from_ref(&library),
                &sidecars(assets),
                false,
            )
            .unwrap_err();
            assert!(
                error.contains("conflicts with asset")
                    || error.contains("duplicate or overlapping"),
                "{error}"
            );
            assert!(!destination.join("libdemo.so").exists());
            assert_eq!(
                std::fs::read(destination.join("runtime/shared.mjs")).unwrap(),
                b"other"
            );
            assert_eq!(std::fs::read(&metadata).unwrap(), original_registry);
            assert_eq!(
                std::fs::read_dir(destination.join("runtime"))
                    .unwrap()
                    .count(),
                1
            );
            assert_no_native_transaction_files(&destination);
        }
    }

    #[test]
    fn native_asset_replacing_an_unowned_file_requires_force() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"demo library").unwrap();
        std::fs::create_dir_all(destination.join("runtime")).unwrap();
        std::fs::write(destination.join("runtime/main.mjs"), b"user file").unwrap();
        let assets = [("runtime/main.mjs", &b"demo"[..])];

        let error = install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &assets),
            std::slice::from_ref(&library),
            &sidecars(&assets),
            false,
        )
        .unwrap_err();
        assert!(error.contains("use --force to replace it"), "{error}");
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"user file"
        );
        assert!(!metadata.exists());

        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &assets),
            std::slice::from_ref(&library),
            &sidecars(&assets),
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"demo"
        );
    }

    #[test]
    fn uninstall_removes_recorded_sidecars_and_only_the_directories_they_empty() {
        let agent = TempAgent::new();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(extension_dir.join("runtime/deep")).unwrap();
        std::fs::write(extension_dir.join("libdemo.so"), b"library").unwrap();
        std::fs::write(extension_dir.join("runtime/main.mjs"), b"main").unwrap();
        std::fs::write(extension_dir.join("runtime/deep/lib.mjs"), b"deep").unwrap();
        std::fs::write(extension_dir.join("runtime/user.mjs"), b"user").unwrap();
        let mut record = prebuilt_record(
            "demo",
            "1.0.0",
            &[
                ("runtime/main.mjs", &b"main"[..]),
                ("runtime/deep/lib.mjs", &b"deep"[..]),
            ],
        );
        record.artifacts = vec!["libdemo.so".into()];
        write_native_packages_at(&agent.metadata_path(), &[record]).unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!extension_dir.join("libdemo.so").exists());
        assert!(!extension_dir.join("runtime/main.mjs").exists());
        assert!(!extension_dir.join("runtime/deep").exists());
        assert_eq!(
            std::fs::read(extension_dir.join("runtime/user.mjs")).unwrap(),
            b"user"
        );
        assert!(!agent.metadata_path().exists());
        assert_no_native_transaction_files(agent.temp.path());
        assert_no_native_transaction_files(&extension_dir);
    }

    #[test]
    fn native_asset_case_only_rename_prunes_the_old_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"library v1").unwrap();
        let first = [("Runtime/main.mjs", &b"worker v1"[..])];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &first),
            std::slice::from_ref(&library),
            &sidecars(&first),
            false,
        )
        .unwrap();
        let second = [("runtime/main.mjs", &b"worker v2"[..])];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.1.0", &second),
            std::slice::from_ref(&library),
            &sidecars(&second),
            false,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"worker v2"
        );
        // The old name is no longer recorded, so it must not survive beside the new one.
        fn store_files(dir: &Path) -> usize {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        store_files(&path)
                    } else {
                        1
                    }
                })
                .sum()
        }
        assert_eq!(store_files(&destination), 3);
    }

    #[test]
    fn native_asset_case_variant_of_an_owned_sidecar_is_not_owned() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"library").unwrap();
        let first = [("Runtime/main.mjs", &b"worker v1"[..])];
        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &first),
            std::slice::from_ref(&library),
            &sidecars(&first),
            false,
        )
        .unwrap();
        std::fs::create_dir_all(destination.join("runtime")).unwrap();
        std::fs::write(destination.join("runtime/main.mjs"), b"user file").unwrap();
        if std::fs::read(destination.join("Runtime/main.mjs")).unwrap() == b"user file" {
            return; // case-insensitive store: the two names are one file, nothing to test
        }

        let second = [("runtime/main.mjs", &b"worker v2"[..])];
        let error = install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.1.0", &second),
            std::slice::from_ref(&library),
            &sidecars(&second),
            false,
        )
        .unwrap_err();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"user file"
        );
        assert_eq!(
            std::fs::read(destination.join("Runtime/main.mjs")).unwrap(),
            b"worker v1"
        );

        install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.1.0", &second),
            std::slice::from_ref(&library),
            &sidecars(&second),
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("runtime/main.mjs")).unwrap(),
            b"worker v2"
        );
        assert!(!destination.join("Runtime").exists());
    }

    #[test]
    fn native_asset_failed_first_install_removes_the_directories_it_created() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"library").unwrap();
        std::fs::create_dir_all(destination.join("keep")).unwrap();
        let assets = [("runtime/deep/x.mjs", &b"x"[..]), ("keep/y.mjs", &b"y"[..])];
        // Replacements activate in order: library, x.mjs, y.mjs, registry.
        let error = install_native_package_at_with_hook(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &assets),
            std::slice::from_ref(&library),
            &sidecars(&assets),
            false,
            |index, _| {
                if index == 2 {
                    Err("injected sidecar activation failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("injected"), "{error}");
        assert!(!destination.join("runtime").exists());
        // A directory that existed before the install is left alone, even though it is empty.
        assert!(destination.join("keep").is_dir());
        assert!(!destination.join("libdemo.so").exists());
        assert!(!metadata.exists());
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    #[test]
    fn native_asset_overlap_within_one_package_is_rejected_before_any_write() {
        // No other package is installed, so only the intra-package check can reject this.
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"demo library").unwrap();
        let assets = [("runtime", &b"file"[..]), ("runtime/own.mjs", &b"own"[..])];

        let error = install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &assets),
            std::slice::from_ref(&library),
            &sidecars(&assets),
            false,
        )
        .unwrap_err();
        assert!(error.contains("duplicate or overlapping"), "{error}");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        assert!(!metadata.exists());
    }

    #[cfg(unix)]
    #[test]
    fn native_asset_parent_links_are_refused_without_writing_through_them() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let outside = temp.path().join("outside");
        let metadata = temp.path().join(NATIVE_PACKAGES_FILE);
        let library = temp.path().join("libdemo.so");
        std::fs::write(&library, b"demo library").unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, destination.join("runtime")).unwrap();
        let assets = [("runtime/main.mjs", &b"demo"[..])];

        let error = install_native_package_with_assets_at(
            &destination,
            &metadata,
            prebuilt_record("demo", "1.0.0", &assets),
            std::slice::from_ref(&library),
            &sidecars(&assets),
            false,
        )
        .unwrap_err();
        assert!(error.contains("is not a plain directory"), "{error}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!destination.join("libdemo.so").exists());
    }
}
