//! Project-local Rust extension packages.
//!
//! A package is a built cdylib plus optional `skills`, `prompts`, and `themes`
//! directories under `.rpi/packages/<name>`. The registry is deliberately
//! separate from the global native package registry.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const REGISTRY: &str = "packages.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub cargo_package: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PackageRegistry {
    #[serde(default)]
    packages: Vec<ProjectPackage>,
}

/// Extension management deliberately exposes only the shared update operation.
pub fn run_extension(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_extension_help();
            0
        }
        Some("update") => update(&args[1..]),
        Some(other) => {
            eprintln!("error: unknown extension command `{other}`");
            print_extension_help();
            2
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            0
        }
        Some("link") => link(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("update") => update(&args[1..]),
        Some("unlink") => unlink(&args[1..]),
        Some(other) => {
            eprintln!("error: unknown package command `{other}`");
            print_help();
            2
        }
    }
}

fn link(args: &[String]) -> i32 {
    let mut source = None;
    let mut cargo_package = None;
    let mut name = None;
    let mut release = true;
    let mut force = false;
    let mut locked = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                i += 1;
                source = value(args, i, "--path");
                if source.is_none() {
                    return 2;
                }
            }
            "--package" | "-P" => {
                i += 1;
                cargo_package = value(args, i, "--package");
                if cargo_package.is_none() {
                    return 2;
                }
            }
            "--name" => {
                i += 1;
                name = value(args, i, "--name");
                if name.is_none() {
                    return 2;
                }
            }
            "--debug" => release = false,
            "--locked" => locked = true,
            "--force" | "-f" => force = true,
            "--help" | "-h" => {
                print_link_help();
                return 0;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown link option `{value}`");
                return 2;
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    eprintln!("error: link accepts one package name");
                    return 2;
                }
            }
        }
        i += 1;
    }
    let project_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let native_root = project_root.join(".rpi");
    let _lease = match crate::resource_management::shared_lease(&native_root) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&native_root) {
        eprintln!("error: could not create {}: {error}", native_root.display());
        return 1;
    }
    let package_lock = match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(native_root.join("packages.lock"))
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("error: could not open project package lock: {error}");
            return 1;
        }
    };
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&package_lock) {
        eprintln!("error: another project package update is running: {error}");
        return 1;
    }
    let package_registry = match read_registry(&project_root) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    // Check an explicit identity before Cargo can execute a build script.
    if let Some(name) = &name {
        if let Err(error) =
            crate::resource_management::legacy_unowned(&native_root, "package", name)
        {
            eprintln!("error: {error}");
            return 1;
        }
    }
    let source = source
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.clone());
    let metadata = match cargo_metadata(&source) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let selected = match select_package(&metadata, cargo_package.as_deref(), &source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let package_name = name.unwrap_or_else(|| selected.name.clone());
    let previous_package = package_registry
        .packages
        .iter()
        .find(|package| package.name == package_name);
    if !valid_name(&package_name) {
        eprintln!("error: invalid package name `{package_name}`");
        return 2;
    }
    if let Err(error) =
        crate::resource_management::legacy_unowned(&native_root, "package", &package_name)
    {
        eprintln!("error: {error}");
        return 1;
    }
    eprintln!("package: building {}...", selected.name);
    let artifact = match build(&selected, release, locked) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let root = project_root.join(".rpi/packages").join(&package_name);
    let ext = root.join("extensions");
    let file = match artifact.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => {
            eprintln!("error: invalid artifact name");
            return 1;
        }
    };
    let target_file = if previous_package.is_some() || (force && ext.join(&file).exists()) {
        match crate::install::versioned_artifact_name(&file, &selected.version, &ext) {
            Ok(name) => name,
            Err(error) => {
                eprintln!("error: could not allocate package artifact: {error}");
                return 1;
            }
        }
    } else {
        file.clone()
    };
    let target = ext.join(&target_file);
    let tmp = ext.join(format!(".{target_file}.tmp"));
    // Cargo can execute build scripts. Recheck the addressed write paths after
    // it returns, before creating directories or removing/copying any artifact.
    let mut guarded_targets = vec![target.clone(), tmp.clone()];
    if target_file != file {
        // A legacy first-install artifact may not be present in the old
        // registry ledger. Keep it inside the validation boundary as well.
        let legacy_target = ext.join(&file);
        let legacy_tmp = ext.join(format!(".{file}.tmp"));
        if legacy_tmp.try_exists().unwrap_or(false) {
            eprintln!("error: could not stage package artifact: staging path already exists");
            return 1;
        }
        guarded_targets.push(legacy_target);
        guarded_targets.push(legacy_tmp);
    }
    if let Some(previous) = previous_package {
        // An upgrade publishes a new artifact, but the old artifact and its
        // staging name still belong to this transaction's safety boundary.
        // Validate them too so a stale reparse point or pre-existing staging
        // file cannot be silently bypassed by the new versioned name.
        for artifact in &previous.artifacts {
            let old_target = ext.join(artifact);
            let old_tmp = ext.join(format!(".{artifact}.tmp"));
            if old_tmp.try_exists().unwrap_or(false) {
                eprintln!("error: could not stage package artifact: staging path already exists");
                return 1;
            }
            guarded_targets.push(old_target);
            guarded_targets.push(old_tmp);
        }
    }
    if let Err(error) =
        crate::resource_management::legacy_unowned(&native_root, "package", &package_name).and_then(
            |_| crate::resource_management::legacy_unowned_targets(&native_root, &guarded_targets),
        )
    {
        eprintln!("error: {error}");
        return 1;
    }
    if target.exists() && !force {
        eprintln!("error: package already exists; use --force");
        return 1;
    }
    if let Err(e) = std::fs::create_dir_all(&ext) {
        eprintln!("error: could not create {}: {e}", ext.display());
        return 1;
    }
    // Never follow/truncate a pre-existing fixed staging path (including a
    // hardlink). Only clean up a temporary file this invocation created.
    let mut staged = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("error: could not stage package artifact: {error}");
            return 1;
        }
    };
    let copied = std::fs::File::open(&artifact)
        .and_then(|mut input| std::io::copy(&mut input, &mut staged))
        .and_then(|_| staged.sync_all());
    drop(staged);
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("error: could not stage package artifact: {error}");
        return 1;
    }
    if force {
        if let Err(e) = std::fs::remove_file(&target) {
            if e.kind() != std::io::ErrorKind::NotFound {
                let _ = std::fs::remove_file(&tmp);
                eprintln!("error: could not replace existing package artifact: {e}");
                return 1;
            }
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("error: could not activate package: {e}");
        return 1;
    }
    let mut registry = match read_registry(&project_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    registry.packages.retain(|p| p.name != package_name);
    registry.packages.push(ProjectPackage {
        name: package_name.clone(),
        version: selected.version,
        source: source.display().to_string(),
        cargo_package: selected.name,
        artifacts: vec![target_file],
    });
    registry.packages.sort_by(|a, b| a.name.cmp(&b.name));
    if let Err(e) = write_registry(&project_root, &registry) {
        eprintln!("error: {e}");
        return 1;
    }
    println!("linked project package `{package_name}`; run `rpi` in this directory to load it");
    0
}

fn list(args: &[String]) -> i32 {
    if !args.is_empty() && args[0] != "--help" && args[0] != "-h" {
        eprintln!("error: unexpected list argument");
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    match read_registry(&cwd) {
        Ok(r) => {
            if r.packages.is_empty() {
                println!("no project packages");
            } else {
                for p in r.packages {
                    println!("{} {} [{}]", p.name, p.version, p.source);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn update(args: &[String]) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    update_at(&cwd, args, update_global_package)
}

fn update_at(
    cwd: &Path,
    args: &[String],
    mut update_global: impl FnMut(&crate::install::InstalledNativePackage, &[String]) -> bool,
) -> i32 {
    let mut name = None;
    let mut link_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--debug" | "--locked" => link_args.push(args[i].clone()),
            "--help" | "-h" => {
                print_update_help();
                return 0;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown update option `{value}`");
                return 2;
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    eprintln!("error: update accepts at most one extension name");
                    return 2;
                }
            }
        }
        i += 1;
    }

    let registry = match read_registry(cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let resource_owned =
        match crate::resource_management::resource_owned_names(&cwd.join(".rpi"), "package") {
            Ok(names) => names,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
    if name
        .as_ref()
        .is_some_and(|name| resource_owned.contains(name))
    {
        eprintln!("error: resource-owned Package; use rpi resource update package NAME --scope project --path PATH");
        return 1;
    }
    let packages = registry
        .packages
        .iter()
        .filter(|package| !resource_owned.contains(&package.name))
        .filter(|package| {
            name.as_deref()
                .map_or(true, |wanted| wanted == package.name)
        })
        .cloned()
        .collect::<Vec<_>>();

    // Global extensions installed by `rpi install` are also packages. This is
    // the path used by crates.io extensions such as rpi-im-message; delegate to
    // the existing transactional installer so artifact and registry updates
    // remain atomic and version resolution stays in one place.
    let global_packages = match crate::install::installed_native_packages_strict() {
        Ok(packages) => packages
            .into_iter()
            .filter(|package| {
                package.supports_registry_update()
                    && name
                        .as_deref()
                        .map_or(true, |wanted| wanted == package.name)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Some(wanted) = name.as_deref() {
        let project_found = registry
            .packages
            .iter()
            .any(|package| package.name == wanted);
        let global_found = global_packages.iter().any(|package| package.name == wanted);
        if !project_found && !global_found {
            eprintln!("error: extension `{wanted}` is not installed");
            return 1;
        }
    }

    if packages.is_empty() && global_packages.is_empty() {
        println!("no project or global extensions to update");
        return 0;
    }

    let mut status = 0;
    for package in packages {
        let package_name = package.name.clone();
        eprintln!("extension: updating project extension `{package_name}`...");
        let mut args = vec![
            "--path".to_string(),
            package.source,
            "--package".to_string(),
            package.cargo_package,
            "--name".to_string(),
            package.name,
            "--force".to_string(),
        ];
        args.extend(link_args.iter().cloned());
        if link(&args) != 0 {
            status = 1;
        }
    }
    for package in global_packages {
        eprintln!("extension: updating global extension `{}`...", package.name);
        if !update_global(&package, &link_args) {
            status = 1;
        }
    }

    status
}

fn global_update_args(
    package: &crate::install::InstalledNativePackage,
    args: &[String],
) -> Vec<String> {
    let mut install_args = vec!["install".into(), package.name.clone()];
    if let Some(source) = &package.prebuilt {
        if source.custom_repository {
            install_args.extend(["--github".into(), source.repository.clone()]);
        }
    } else {
        install_args.push("--build".into());
    }
    if package.prebuilt.is_none() && args.iter().any(|arg| arg == "--locked") {
        install_args.push("--locked".into());
    }
    install_args
}

fn update_global_package(
    package: &crate::install::InstalledNativePackage,
    args: &[String],
) -> bool {
    let name = &package.name;
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve rpi executable: {error}");
            return false;
        }
    };
    let mut command = std::process::Command::new(executable);
    command.args(global_update_args(package, args));
    match command.status() {
        Ok(exit) if exit.success() => true,
        Ok(exit) => {
            eprintln!("error: updating global extension `{name}` failed with {exit}");
            false
        }
        Err(error) => {
            eprintln!("error: could not run rpi install for `{name}`: {error}");
            false
        }
    }
}

fn unlink(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        eprintln!("error: missing package name");
        return 2;
    };
    if args.len() != 1 || !valid_name(name) {
        eprintln!("error: invalid package name or arguments");
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let native_root = cwd.join(".rpi");
    let _lease = match crate::resource_management::mutation_lease(&native_root) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Err(error) = crate::resource_management::legacy_unowned(&native_root, "package", name) {
        eprintln!("error: {error}");
        return 1;
    }
    let mut registry = match read_registry(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if !registry.packages.iter().any(|p| p.name == *name) {
        eprintln!("error: project package `{name}` is not installed");
        return 1;
    }
    let dir = cwd.join(".rpi/packages").join(name);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("error: could not remove package: {e}");
        return 1;
    }
    registry.packages.retain(|p| p.name != *name);
    if let Err(e) = write_registry(&cwd, &registry) {
        eprintln!("error: {e}");
        return 1;
    }
    println!("unlinked project package `{name}`");
    0
}

#[derive(Debug, Clone)]
struct CargoPackage {
    name: String,
    version: String,
    manifest: PathBuf,
    target: String,
}
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
}
#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    version: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}
#[derive(Deserialize)]
struct Target {
    name: String,
    crate_types: Vec<String>,
}

fn cargo_metadata(cwd: &Path) -> Result<Metadata, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not execute cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("invalid cargo metadata output: {e}"))
}
fn select_package(
    m: &Metadata,
    requested: Option<&str>,
    cwd: &Path,
) -> Result<CargoPackage, String> {
    let mut found = Vec::new();
    for p in &m.packages {
        if requested.is_some_and(|n| n != p.name) {
            continue;
        }
        for t in &p.targets {
            if t.crate_types.iter().any(|k| k == "cdylib") {
                found.push(CargoPackage {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    manifest: p.manifest_path.clone(),
                    target: t.name.clone(),
                });
            }
        }
    }
    if requested.is_none() {
        // Cargo returns normalized manifest paths. --path may contain `..` or
        // a Windows extended-length prefix; compare normalized paths on both
        // sides when choosing among several provider packages in a workspace.
        let source = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let mut local = found
            .iter()
            .filter(|p| {
                p.manifest.parent().is_some_and(|root| {
                    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                    source.starts_with(root)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if local.len() == 1 {
            return Ok(local.remove(0));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err("no cdylib package found; use --package to select one".into()),
        _ => Err(format!(
            "multiple cdylib packages found: {}; use --package",
            found
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
fn build(p: &CargoPackage, release: bool, locked: bool) -> Result<PathBuf, String> {
    let mut c = Command::new("cargo");
    c.args(["build", "--manifest-path"]).arg(&p.manifest).args([
        "--package",
        &p.name,
        "--message-format=json-render-diagnostics",
    ]);
    if release {
        c.arg("--release");
    }
    if locked {
        c.arg("--locked");
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = c
        .output()
        .map_err(|e| format!("could not execute cargo build: {e}"))?;
    let mut artifact = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("reason").and_then(|v| v.as_str()) == Some("compiler-artifact")
                && v.get("target")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    == Some(&p.target)
            {
                artifact = v
                    .get("filenames")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .find(|p| is_dynamic(p));
            }
        }
    }
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(format!("cargo build exited with {}", out.status));
    }
    artifact.ok_or_else(|| "Cargo did not report a cdylib artifact".into())
}
fn is_dynamic(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .as_deref(),
        Some("dll" | "so" | "dylib")
    )
}
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn value(args: &[String], i: usize, flag: &str) -> Option<String> {
    match args.get(i) {
        Some(v) if !v.starts_with('-') => Some(v.clone()),
        _ => {
            eprintln!("error: {flag} requires a value");
            None
        }
    }
}
fn read_registry(cwd: &Path) -> Result<PackageRegistry, String> {
    let path = cwd.join(".rpi").join(REGISTRY);
    let registry = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| format!("invalid project package registry: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PackageRegistry::default()),
        Err(e) => return Err(e.to_string()),
    };
    validate_registry(&registry)?;
    Ok(registry)
}

fn validate_registry(registry: &PackageRegistry) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for package in &registry.packages {
        if !valid_name(&package.name) || !names.insert(package.name.to_ascii_lowercase()) {
            return Err(format!(
                "invalid or duplicate project package name `{}`",
                package.name
            ));
        }
        let mut artifacts = std::collections::HashSet::new();
        for artifact in &package.artifacts {
            let path = Path::new(artifact);
            if path.components().count() != 1
                || path.file_name().and_then(|value| value.to_str()) != Some(artifact)
                || !is_dynamic(path)
                || !artifacts.insert(artifact.to_ascii_lowercase())
            {
                return Err(format!(
                    "invalid or duplicate project package artifact `{artifact}`"
                ));
            }
        }
    }
    Ok(())
}
fn write_registry(cwd: &Path, r: &PackageRegistry) -> Result<(), String> {
    let dir = cwd.join(".rpi");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(REGISTRY);
    let tmp = dir.join(".packages.json.tmp");
    let bytes = serde_json::to_vec_pretty(r).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}
pub fn selected_project_package(cwd: &Path, name: &str) -> Result<ProjectPackage, String> {
    selected_package_at(&cwd.join(".rpi"), name)
}

pub(crate) fn selected_package_at(root: &Path, name: &str) -> Result<ProjectPackage, String> {
    if !valid_name(name) {
        return Err("invalid main package name".into());
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    let limit = crate::resource_selection::MAX_STATE_BYTES;
    std::fs::File::open(root.join(REGISTRY))
        .map_err(|e| e.to_string())?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("project package registry exceeds 256 KiB".into());
    }
    let registry: PackageRegistry = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid project package registry: {e}"))?;
    validate_registry(&registry)?;
    registry
        .packages
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("main package `{name}` not in .rpi/packages.json"))
}

pub fn project_package_dirs(cwd: &Path, kind: &str) -> Vec<PathBuf> {
    match read_registry(cwd) {
        Ok(registry) => registry
            .packages
            .into_iter()
            .map(|p| cwd.join(".rpi/packages").join(p.name).join(kind))
            .collect(),
        Err(error) => {
            eprintln!("warning: could not load project package registry: {error}");
            Vec::new()
        }
    }
}

/// Resolve the active project package extension files. Package directories
/// are not scanned because upgrades retain retired versioned artifacts for
/// already-running hosts.
pub fn active_project_package_artifacts(cwd: &Path) -> Vec<PathBuf> {
    let Ok(registry) = read_registry(cwd) else {
        return Vec::new();
    };
    registry
        .packages
        .into_iter()
        .flat_map(|package| {
            package.artifacts.into_iter().map(move |artifact| {
                cwd.join(".rpi/packages")
                    .join(package.name.clone())
                    .join("extensions")
                    .join(artifact)
            })
        })
        .collect()
}
fn print_extension_help() {
    println!("Usage: rpi extension <command>\n\nCommands:\n  update [name] [--debug] [--locked]  Update project and global installed extensions\n\nSee `rpi extension update --help` for options.");
}

pub fn print_help() {
    println!("Usage: rpi package <command>\n\nCommands:\n  link [name] [--path <dir>] [--package <name>] [--force]  Build and link a project-local Rust extension\n  list                                                   List project packages\n  update [name] [--debug] [--locked]                    Legacy compatibility alias for `rpi extension update`\n  unlink <name>                                          Remove a project package");
}
fn print_link_help() {
    println!("Usage: rpi package link [name] [options]\n\nOptions:\n  --path <dir>       Extension source directory (default: current directory)\n  --package, -P <n>  Select a Cargo cdylib package\n  --debug            Build debug artifacts instead of release\n  --locked           Pass --locked to Cargo\n  --force, -f        Replace an existing package");
}
fn print_update_help() {
    println!("Usage: rpi extension update [name] [options]\n\nOptions:\n  --debug            Build debug artifacts instead of release\n  --locked           Pass --locked to Cargo\n  --help, -h         Show this help\n\nWithout a name, all project and global installed extensions are updated.\nLegacy compatibility alias: rpi package update [name] [options]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_package(name: &str, source: Option<&str>) -> crate::install::InstalledNativePackage {
        crate::install::InstalledNativePackage {
            name: name.into(),
            version: "0.1.11".into(),
            source: source.map(str::to_owned),
            prebuilt: None,
            artifacts: Vec::new(),
        }
    }

    fn prebuilt_package(custom_repository: bool) -> crate::install::InstalledNativePackage {
        serde_json::from_value(serde_json::json!({
            "name": "rpi-im-message", "version": "0.1.11",
            "source": "prebuilt:https://github.com/owner/extensions",
            "prebuilt": {
                "repository": "https://github.com/owner/extensions",
                "target": "x86_64-pc-windows-msvc",
                "manifest_sha256": "a".repeat(64), "source_commit": "b".repeat(40),
                "sdk_version": "0.4.0", "plugin_abi": 4,
                "archive_sha256": "c".repeat(64), "library_sha256": "d".repeat(64),
                "custom_repository": custom_repository
            }
        }))
        .unwrap()
    }

    fn with_registry(data: &[u8], test: impl FnOnce(&Path)) {
        struct RestoreConfigDir(Option<std::ffi::OsString>);
        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }
        let _guard = crate::config::test_support::env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, agent.path());
        std::fs::write(agent.path().join("native-packages.json"), data).unwrap();
        test(project.path());
    }

    #[test]
    fn update_includes_prebuilt_packages_and_preserves_install_backends() {
        let records = vec![
            prebuilt_package(false),
            native_package("cargo-extension", None),
            native_package("local-extension", Some("/dev/local-extension")),
        ];
        with_registry(&serde_json::to_vec(&records).unwrap(), |cwd| {
            // Reproduce the empty project registry from the bug report. Neither
            // the prebuilt source marker nor the lack of project links should
            // hide an installed remote package.
            let mut commands = Vec::new();
            assert_eq!(
                update_at(cwd, &[], |package, args| {
                    commands.push(global_update_args(package, args));
                    true
                }),
                0
            );
            assert_eq!(
                commands,
                [
                    vec!["install", "rpi-im-message"],
                    vec!["install", "cargo-extension", "--build"],
                ]
            );

            for wanted in ["rpi-im-message", "cargo-extension"] {
                commands.clear();
                assert_eq!(
                    update_at(cwd, &[wanted.into(), "--locked".into()], |package, args| {
                        commands.push(global_update_args(package, args));
                        true
                    }),
                    0
                );
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0][1], wanted);
                assert_eq!(
                    commands[0].iter().any(|arg| arg == "--locked"),
                    wanted == "cargo-extension"
                );
            }
            for wanted in ["local-extension", "missing-extension"] {
                assert_eq!(
                    update_at(cwd, &[wanted.into()], |_, _| {
                        panic!("local or unknown packages must not trigger a remote install")
                    }),
                    1
                );
            }
        });
    }

    #[test]
    fn update_preserves_explicit_github_repository() {
        let record = prebuilt_package(true);
        with_registry(&serde_json::to_vec(&[record]).unwrap(), |cwd| {
            let mut commands = Vec::new();
            assert_eq!(
                update_at(cwd, &[], |package, args| {
                    commands.push(global_update_args(package, args));
                    true
                }),
                0
            );
            assert_eq!(
                commands,
                [vec![
                    "install",
                    "rpi-im-message",
                    "--github",
                    "https://github.com/owner/extensions"
                ]]
            );
        });
    }

    #[test]
    fn update_continues_after_a_failed_package_and_returns_failure() {
        let records = vec![
            prebuilt_package(false),
            native_package("other-extension", None),
        ];
        with_registry(&serde_json::to_vec(&records).unwrap(), |cwd| {
            let mut visited = Vec::new();
            assert_eq!(
                update_at(cwd, &[], |package, _| {
                    visited.push(package.name.clone());
                    package.name != "rpi-im-message"
                }),
                1
            );
            assert_eq!(visited, ["rpi-im-message", "other-extension"]);
        });
    }

    #[test]
    fn update_rejects_corrupt_global_metadata_before_building_project_packages() {
        with_registry(b"invalid json", |cwd| {
            write_registry(
                cwd,
                &PackageRegistry {
                    packages: vec![ProjectPackage {
                        name: "project-extension".into(),
                        version: "0.1.0".into(),
                        source: "/missing/source".into(),
                        cargo_package: "project-extension".into(),
                        artifacts: Vec::new(),
                    }],
                },
            )
            .unwrap();
            assert_eq!(update_at(cwd, &[], |_, _| panic!("invalid registry")), 1);
            assert!(!cwd.join(".rpi/packages/project-extension").exists());
        });
    }

    #[test]
    fn package_names_reject_path_traversal() {
        assert!(valid_name("review-tools"));
        assert!(!valid_name("../review-tools"));
        assert!(!valid_name("review/tools"));
        assert!(!valid_name(""));
    }

    #[test]
    fn update_help_is_available_without_a_registry() {
        print_update_help();
    }

    #[test]
    fn dynamic_library_extensions_are_platform_neutral() {
        assert!(is_dynamic(Path::new("plugin.dll")));
        assert!(is_dynamic(Path::new("libplugin.so")));
        assert!(is_dynamic(Path::new("libplugin.dylib")));
        assert!(!is_dynamic(Path::new("plugin.exe")));
    }
}
