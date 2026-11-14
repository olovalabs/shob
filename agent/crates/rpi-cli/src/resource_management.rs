//! Native local snapshot management. No compilation, dlopen, provider or network.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const RECEIPTS: &str = "resource-receipts.json";
const JOURNAL: &str = "resource-transaction.json";
const MAX_META: usize = 256 * 1024;
const MAX_FILES: usize = 2048;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Static protocol description only: never resolves a root or opens metadata.
pub(crate) fn capabilities() -> Value {
    json!({"version":1,"command":"resource","kinds":["package","skill","extension"],
        "scopes":["project","global"],"explicitScopeRequired":true,
        "operations":["list","inspect","install","update","reinstall","remove","recover"],
        "localSnapshotsOnly":true,"build":false,"network":false,"hotApply":false,
        "bounds":{"maxMetadataBytes":MAX_META,"maxResultBytes":MAX_META,"maxResourcesPerScope":128,
            "maxInventoryEntries":MAX_FILES,"maxDepth":32,"maxFileBytes":MAX_FILE,"maxTotalBytes":MAX_BYTES}})
}

type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Project,
    Global,
}
impl Scope {
    pub fn is_project(&self) -> bool {
        *self == Self::Project
    }
    pub fn root(self, cwd: &Path) -> Result<PathBuf> {
        let logical = match self {
            Self::Project => cwd.join(".rpi"),
            Self::Global => crate::config::agent_dir().map_err(err)?,
        };
        resolve_scope_root(&logical)
    }
}

/// Resolve only the external prefix of an owned scope root.
///
/// The root itself is deliberately appended after canonicalization so a link
/// at `.rpi`/`agent` remains visible to `checked_path`. Missing parents are
/// represented by a tail appended to the nearest existing parent; this is
/// path resolution only and never creates directories or reads metadata files.
fn resolve_scope_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return Err("resource root must be absolute".into());
    }
    if root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("resource path traversal".into());
    }
    let Some(name) = root.file_name() else {
        // A filesystem root has no external prefix to resolve.
        checked_path(root)?;
        return Ok(root.to_path_buf());
    };
    #[cfg(windows)]
    checked_scope_tail_component(name)?;
    let mut parent = root.parent().ok_or("resource root has no parent")?;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    parent
                        .file_name()
                        .ok_or("resource root has no existing parent")?
                        .to_os_string(),
                );
                parent = parent
                    .parent()
                    .ok_or("resource root has no existing parent")?;
            }
            Err(e) => {
                return Err(format!(
                    "checking resource root parent {}: {e}",
                    parent.display()
                ))
            }
        }
    }
    let mut resolved = fs::canonicalize(parent).map_err(err)?;
    for component in missing.into_iter().rev() {
        #[cfg(windows)]
        checked_scope_tail_component(&component)?;
        resolved.push(component);
    }
    resolved.push(name);
    checked_path(&resolved)?;
    Ok(resolved)
}

#[cfg(windows)]
fn checked_scope_tail_component(component: &std::ffi::OsStr) -> Result<()> {
    let name = component
        .to_str()
        .ok_or("non-UTF-8 resource scope component")?;
    // canonicalize returns an extended-length path. Appending a DOS name
    // with trailing dots/spaces, a device name, or an ADS suffix could then
    // address a different directory than loaders using the original path.
    if name.ends_with(['.', ' ']) || name.contains(':') {
        return Err("ambiguous Windows resource scope component".into());
    }
    reserved_identity(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Package,
    Skill,
    Extension,
}
impl Kind {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "package" => Ok(Self::Package),
            "skill" => Ok(Self::Skill),
            "extension" => Ok(Self::Extension),
            _ => Err("kind must be package, skill or extension".into()),
        }
    }
    fn dir(self) -> &'static str {
        match self {
            Self::Package => "packages",
            Self::Skill => "skills",
            Self::Extension => "extensions",
        }
    }
    fn target(self, name: &str) -> PathBuf {
        Path::new(self.dir()).join(if self == Self::Extension {
            format!("{name}.{}", std::env::consts::DLL_EXTENSION)
        } else {
            name.into()
        })
    }
}
fn identifier(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 96
        || !s.as_bytes()[0].is_ascii_alphanumeric()
        || !s
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        || s.ends_with('.')
    {
        return Err("invalid resource identity (portable ASCII name required)".into());
    }
    reserved_identity(s)
}

// Existing native link records allow leading '_' and '.', unlike new managed
// identities. Accept that grammar only for safe on-disk package components.
fn legacy_package_identity(s: &str) -> Result<()> {
    if !crate::packages::valid_name(s) {
        return Err("invalid native package identity".into());
    }
    relative(Path::new(s))?;
    reserved_identity(s)
}

fn reserved_identity(s: &str) -> Result<()> {
    let stem = s.split('.').next().unwrap().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err("reserved resource identity".into());
    }
    Ok(())
}
fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("unsafe relative resource path".into());
    }
    for part in path {
        let s = part.to_str().ok_or("non-UTF-8 resource path")?;
        if s.len() > 128 || s.contains([':', '\\']) || s.ends_with(['.', ' ']) {
            return Err("unsafe resource filename".into());
        }
    }
    Ok(())
}
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
/// Validate every existing ancestor too, including Windows junctions.
pub(crate) fn checked_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err("resource path must be absolute".into());
    }
    let mut current = PathBuf::new();
    for c in path.components() {
        if matches!(c, Component::ParentDir) {
            return Err("resource path traversal".into());
        }
        current.push(c);
        // Prefix alone is not an addressable ancestor (notably `\\\\?\\C:`).
        // Inspect the filesystem root once RootDir has made it absolute.
        if matches!(c, Component::Prefix(_)) || !current.is_absolute() {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(m) if is_link(&m) => {
                return Err(format!(
                    "links/reparse points are not owned resource paths: {}",
                    current.display()
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "checking resource ancestor {}: {e}",
                    current.display()
                ))
            }
        }
    }
    Ok(())
}
fn read_bytes(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    checked_path(path)?;
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(e)),
    };
    if !file.metadata().map_err(err)?.is_file() {
        return Err("resource metadata is not a regular file".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() > limit {
        return Err(format!("resource metadata exceeds {limit} bytes"));
    }
    Ok(Some(bytes))
}
fn read_json<T: for<'a> Deserialize<'a> + Default>(path: &Path) -> Result<T> {
    match read_bytes(path, MAX_META)? {
        None => Ok(T::default()),
        Some(b) => {
            serde_json::from_slice(&b).map_err(|e| format!("invalid {}: {e}", path.display()))
        }
    }
}
fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let b = serde_json::to_vec_pretty(value).map_err(err)?;
    if b.len() > MAX_META {
        Err("resource metadata/output exceeds 256 KiB".into())
    } else {
        Ok(b)
    }
}

/// Stable lock inode: never removed, no PID heuristics, no blocking wait.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}
#[derive(Debug)]
enum LeaseError {
    Contended,
    Other(String),
}
impl From<String> for LeaseError {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}
impl From<LeaseError> for String {
    fn from(error: LeaseError) -> Self {
        match error {
            LeaseError::Contended => {
                "resource scope is in use (host or mutation lease); retry after owners exit".into()
            }
            LeaseError::Other(e) => format!("resource lease error: {e}"),
        }
    }
}
fn lock_error(error: std::io::Error) -> LeaseError {
    let busy = fs2::lock_contended_error();
    if error.raw_os_error().is_some() && error.raw_os_error() == busy.raw_os_error() {
        LeaseError::Contended
    } else {
        LeaseError::Other(error.to_string())
    }
}

/// All processes addressing the same resource scope must lock the same inode.
/// Keep that inode in application data rather than creating a project folder.
fn lease_path(root: &Path) -> PathBuf {
    let hash = Sha256::digest(root.as_os_str().to_string_lossy().as_bytes());
    let name = hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    crate::session::kathamo_session_dir()
        .parent()
        .expect("session directory has a parent")
        .join("resource-leases")
        .join(name)
}

impl Lease {
    fn acquire(
        root: &Path,
        exclusive: bool,
        create: bool,
    ) -> std::result::Result<Option<Self>, LeaseError> {
        checked_path(root)?;
        let path = lease_path(root);
        checked_path(&path)?;
        if create {
            fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| "resource lease has no parent".to_string())?,
            )
            .map_err(err)?;
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LeaseError::Other(err(e))),
        };
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(&file)
        } else {
            fs2::FileExt::try_lock_shared(&file)
        };
        result.map_err(lock_error)?;
        Ok(Some(Self { _file: file }))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}
pub(crate) fn mutation_lease(root: &Path) -> Result<Lease> {
    let lease = Lease::acquire(root, true, true)?.ok_or("missing resource lease")?;
    fs::create_dir_all(root).map_err(err)?;
    ensure_complete(root)?;
    Ok(lease)
}

/// Hold a shared resource lease while updating native package metadata. Native
/// package installation is append/version based and therefore safe alongside
/// live hosts, but it must still exclude resource mutations that could race
/// with the registry/receipt ownership checks.
pub(crate) fn shared_lease(root: &Path) -> Result<Lease> {
    let lease = Lease::acquire(root, false, true)?.ok_or("missing resource lease")?;
    ensure_complete(root)?;
    Ok(lease)
}

fn ensure_complete(root: &Path) -> Result<()> {
    if read_bytes(&root.join(JOURNAL), MAX_META)?.is_some() {
        return Err(
            "incomplete resource transaction; run rpi resource recover with this scope".into(),
        );
    }
    Ok(())
}
pub(crate) fn host_leases(cwd: &Path) -> Result<Arc<Vec<Lease>>> {
    let mut roots = vec![Scope::Project.root(cwd)?, Scope::Global.root(cwd)?];
    roots.sort();
    roots.dedup();
    let mut leases = Vec::new();
    for root in roots {
        leases.push(shared_lease(&root)?);
    }
    Ok(Arc::new(leases))
}
fn observation_lease(root: &Path) -> Result<(Option<Lease>, bool)> {
    // Retain the idle exclusive probe, or a shared lease alongside hosts.
    // A mutator's exclusive lease refuses both; no unguarded half-state read.
    match Lease::acquire(root, true, false) {
        Ok(lease) => Ok((lease, false)),
        Err(LeaseError::Contended) => Ok((Lease::acquire(root, false, false)?, true)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ResourceFile {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Inventory {
    files: Vec<ResourceFile>,
    directories: Vec<PathBuf>,
    sha256: String,
}
#[derive(Default)]
struct InventoryBudget {
    entries: usize,
    bytes: u64,
}
fn inventory(path: &Path) -> Result<Inventory> {
    inventory_bounded(path, &mut InventoryBudget::default())
}
fn inventory_bounded(path: &Path, budget: &mut InventoryBudget) -> Result<Inventory> {
    checked_path(path)?;
    let mut files = Vec::new();
    let mut directories = Vec::new();
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut Vec<ResourceFile>,
        dirs: &mut Vec<PathBuf>,
        budget: &mut InventoryBudget,
        depth: usize,
    ) -> Result<()> {
        if depth > 32 {
            return Err("resource recursion depth exceeds 32".into());
        }
        if budget.entries >= MAX_FILES {
            return Err("resource inventory exceeds 2048 entries".into());
        }
        budget.entries += 1;
        let meta = fs::symlink_metadata(path).map_err(err)?;
        if is_link(&meta) {
            return Err(format!(
                "resource link/reparse point rejected: {}",
                path.display()
            ));
        }
        let rel = path.strip_prefix(root).map_err(err)?;
        if !rel.as_os_str().is_empty() {
            relative(rel)?;
        }
        if meta.is_dir() {
            if !rel.as_os_str().is_empty() {
                dirs.push(rel.to_path_buf());
            }
            let remaining = MAX_FILES - budget.entries;
            let mut entries = Vec::new();
            for entry in fs::read_dir(path).map_err(err)?.take(remaining + 1) {
                if entries.len() == remaining {
                    return Err("resource directory exceeds remaining entry budget".into());
                }
                entries.push(entry.map_err(err)?);
            }
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                walk(root, &e.path(), files, dirs, budget, depth + 1)?;
            }
        } else if meta.is_file() {
            if meta.len() > MAX_FILE || budget.bytes + meta.len() > MAX_BYTES {
                return Err("resource files exceed byte limit (64 MiB/file, 256 MiB total)".into());
            }
            let mut input = File::open(path).map_err(err)?;
            let mut hash = Sha256::new();
            let mut size = 0;
            let mut buf = [0; 16384];
            loop {
                let n = input.read(&mut buf).map_err(err)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                budget.bytes += n as u64;
                if size > MAX_FILE || budget.bytes > MAX_BYTES {
                    return Err("resource changed/exceeded byte limit".into());
                }
                hash.update(&buf[..n]);
            }
            if size != meta.len() {
                return Err("resource changed while reading".into());
            }
            files.push(ResourceFile {
                path: if rel.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    rel.into()
                },
                bytes: size,
                sha256: format!("{:x}", hash.finalize()),
            });
        } else {
            return Err("special resource file rejected".into());
        }
        Ok(())
    }
    walk(path, path, &mut files, &mut directories, budget, 0)?;
    let sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&files, &directories)).map_err(err)?)
    );
    Ok(Inventory {
        files,
        directories,
        sha256,
    })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    kind: Kind,
    name: String,
    source: PathBuf,
    version: Option<String>,
    target: PathBuf,
    inventory: Inventory,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipts {
    #[serde(default)]
    resources: Vec<Receipt>,
}
fn receipts(root: &Path) -> Result<Receipts> {
    let r: Receipts = read_json(&root.join(RECEIPTS))?;
    let mut keys = BTreeSet::new();
    if r.resources.len() > 128 {
        return Err("resource receipts exceed 128 resources per scope".into());
    }
    for item in &r.resources {
        identifier(&item.name)?;
        if item.version.as_ref().is_some_and(|v| {
            v.trim().is_empty() || v.len() > 128 || v.chars().any(char::is_control)
        }) {
            return Err("invalid resource receipt version".into());
        }
        if item.target != item.kind.target(&item.name)
            || !item.source.is_absolute()
            || !keys.insert(item.target.to_string_lossy().to_ascii_lowercase())
        {
            return Err("unsafe/duplicate resource receipt".into());
        }
        checked_path(&root.join(&item.target))?;
        if item.inventory.files.len() + item.inventory.directories.len() > MAX_FILES {
            return Err("oversized receipt inventory".into());
        }
        for file in &item.inventory.files {
            if file.path != Path::new(".") {
                relative(&file.path)?;
            }
        }
        for dir in &item.inventory.directories {
            relative(dir)?;
        }
    }
    Ok(r)
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packages {
    #[serde(default)]
    packages: Vec<crate::packages::ProjectPackage>,
}
fn packages(root: &Path) -> Result<Packages> {
    let p: Packages = read_json(&root.join("packages.json"))?;
    let mut names = BTreeSet::new();
    for item in &p.packages {
        legacy_package_identity(&item.name)?;
        if !names.insert(item.name.to_ascii_lowercase()) {
            return Err("duplicate native package identity".into());
        }
        let mut artifacts = BTreeSet::new();
        for a in &item.artifacts {
            relative(Path::new(a))?;
            reserved_identity(a)?;
            if Path::new(a).components().count() != 1 || !artifacts.insert(a.to_ascii_lowercase()) {
                return Err("unsafe/duplicate native artifact".into());
            }
        }
    }
    Ok(p)
}
pub(crate) fn legacy_unowned(root: &Path, kind: &str, name: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    if receipts(root)?
        .resources
        .iter()
        .any(|r| r.kind == kind && r.name.eq_ignore_ascii_case(name))
    {
        return Err("resource is managed by rpi resource; use its update/remove commands".into());
    }
    if kind == Kind::Package {
        legacy_package_identity(name)?;
        let package = root.join(kind.target(name));
        legacy_unowned_targets(root, &[package.clone(), package.join("extensions")])?;
    }
    Ok(())
}
fn path_identity(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(err)?.join(path)
    };
    checked_path(&path)?;
    // Existing components are canonicalized; absent leaves still compare by
    // their absolute addressed path. Reparse paths are rejected, not followed.
    let mut ancestor = path.as_path();
    let mut tail = Vec::new();
    while !ancestor.try_exists().map_err(err)? {
        tail.push(
            ancestor
                .file_name()
                .ok_or("no path ancestor")?
                .to_os_string(),
        );
        ancestor = ancestor.parent().ok_or("no path ancestor")?;
    }
    let mut identity = fs::canonicalize(ancestor).map_err(err)?;
    for component in tail.into_iter().rev() {
        identity.push(component);
    }
    if cfg!(windows) {
        Ok(PathBuf::from(identity.to_string_lossy().to_lowercase()))
    } else {
        Ok(identity)
    }
}
pub(crate) fn resource_owned_names(root: &Path, kind: &str) -> Result<BTreeSet<String>> {
    let kind = Kind::parse(kind)?;
    Ok(receipts(root)?
        .resources
        .into_iter()
        .filter(|r| r.kind == kind)
        .map(|r| r.name)
        .collect())
}
pub(crate) fn legacy_unowned_targets(root: &Path, targets: &[PathBuf]) -> Result<()> {
    let targets = targets
        .iter()
        .map(|t| path_identity(t))
        .collect::<Result<Vec<_>>>()?;
    for r in receipts(root)?.resources {
        let owned = path_identity(&root.join(&r.target))?;
        if targets.iter().any(|t| t == &owned || t.starts_with(&owned)) {
            return Err("target owned by rpi resource; refusing legacy replacement/removal".into());
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    target: PathBuf,
    old: Option<Inventory>,
    new: Option<Inventory>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    directory: String,
    changes: Vec<Change>,
}
fn transaction_target(p: &Path) -> Result<()> {
    relative(p)?;
    if p == Path::new(RECEIPTS) || p == Path::new("packages.json") {
        return Ok(());
    }
    let parts = p
        .iter()
        .map(|p| p.to_str().ok_or("non-UTF8 target"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if parts.len() != 2 || !matches!(parts[0], "packages" | "skills" | "extensions") {
        return Err("unsafe transaction target".into());
    }
    identifier(parts[1])
}
fn remove_path(path: &Path) -> Result<()> {
    checked_path(path)?;
    let m = fs::symlink_metadata(path).map_err(err)?;
    if m.is_dir() {
        fs::remove_dir_all(path).map_err(err)
    } else if m.is_file() {
        fs::remove_file(path).map_err(err)
    } else {
        Err("unsafe removal".into())
    }
}
fn rollback(root: &Path, j: &Journal) -> Result<()> {
    let dir = root.join(&j.directory);
    checked_path(&dir)?;
    // Validate the complete recovery plan before changing any file.
    for (i, c) in j.changes.iter().enumerate() {
        transaction_target(&c.target)?;
        let target = root.join(&c.target);
        let backup = dir.join(format!("old-{i}"));
        if backup.try_exists().map_err(err)? {
            if Some(inventory(&backup)?) != c.old {
                return Err("transaction backup changed; manual recovery required".into());
            }
            if target.try_exists().map_err(err)? && Some(inventory(&target)?) != c.new {
                return Err("transaction target changed; refusing recovery deletion".into());
            }
        } else if target.try_exists().map_err(err)? {
            let observed = inventory(&target)?;
            if c.old.as_ref() != Some(&observed) && c.new.as_ref() != Some(&observed) {
                return Err("transaction target differs from old/new inventory".into());
            }
            if c.old.is_some() && c.old.as_ref() != Some(&observed) {
                return Err("missing rollback backup; manual recovery required".into());
            }
        } else if c.old.is_some() {
            return Err("missing original/backup; manual recovery required".into());
        }
    }
    for (i, c) in j.changes.iter().enumerate().rev() {
        let target = root.join(&c.target);
        let backup = dir.join(format!("old-{i}"));
        if backup.try_exists().map_err(err)? {
            if target.try_exists().map_err(err)? {
                remove_path(&target)?;
            }
            fs::rename(&backup, &target).map_err(err)?;
        } else if c.old.is_none() && target.try_exists().map_err(err)? {
            remove_path(&target)?;
        }
    }
    // Journal last: a failed rollback stays diagnosable and retryable.
    fs::remove_file(root.join(JOURNAL)).map_err(err)?;
    fs::remove_dir_all(dir).map_err(err)
}
fn recover(root: &Path) -> Result<bool> {
    let Some(bytes) = read_bytes(&root.join(JOURNAL), MAX_META)? else {
        return Ok(false);
    };
    let j: Journal = serde_json::from_slice(&bytes).map_err(err)?;
    let suffix = j
        .directory
        .strip_prefix(".resource-txn-")
        .ok_or("invalid transaction directory")?;
    uuid::Uuid::parse_str(suffix).map_err(err)?;
    if j.changes.len() > 3 {
        return Err("oversized transaction plan".into());
    }
    let mut seen = BTreeSet::new();
    for c in &j.changes {
        transaction_target(&c.target)?;
        if !seen.insert(&c.target) {
            return Err("duplicate transaction target".into());
        }
    }
    rollback(root, &j)?;
    Ok(true)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(err)
}
fn copy_snapshot(source: &Path, target: &Path, observed: &Inventory) -> Result<()> {
    fn copy_file(source: &Path, target: &Path, expected: u64) -> Result<()> {
        checked_path(source)?;
        let input = File::open(source).map_err(err)?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            // Only ordinary rwx permissions; never copy setuid/setgid/sticky.
            input.metadata().map_err(err)?.permissions().mode() & 0o777
        };
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(err)?;
        let copied = std::io::copy(&mut input.take(expected + 1), &mut output).map_err(err)?;
        if copied != expected {
            return Err("resource changed during bounded copy".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(err)?;
        }
        output.sync_all().map_err(err)
    }
    if source.is_file() {
        copy_file(
            source,
            target,
            observed
                .files
                .first()
                .ok_or("missing file inventory")?
                .bytes,
        )?;
    } else {
        fs::create_dir(target).map_err(err)?;
        for d in &observed.directories {
            fs::create_dir_all(target.join(d)).map_err(err)?;
        }
        for f in &observed.files {
            let to = target.join(&f.path);
            if let Some(p) = to.parent() {
                fs::create_dir_all(p).map_err(err)?;
            }
            copy_file(&source.join(&f.path), &to, f.bytes)?;
        }
    }
    if inventory(target)? != *observed || inventory(source)? != *observed {
        return Err("resource source changed during snapshot copy".into());
    }
    Ok(())
}
/// Small same-filesystem transaction; injection is unit-test-only, never an env backdoor.
fn publish(
    root: &Path,
    changes: Vec<(PathBuf, Option<PathBuf>)>,
    stage: tempfile::TempDir,
    mut before: impl FnMut(usize) -> Result<()>,
) -> Result<Vec<String>> {
    let directory = stage
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid staging directory")?
        .to_owned();
    let mut j = Journal {
        directory,
        changes: Vec::new(),
    };
    for (target, new) in &changes {
        transaction_target(target)?;
        let path = root.join(target);
        checked_path(&path)?;
        j.changes.push(Change {
            target: target.clone(),
            old: if path.try_exists().map_err(err)? {
                Some(inventory(&path)?)
            } else {
                None
            },
            new: new.as_ref().map(|p| inventory(p)).transpose()?,
        });
    }
    let journal_stage = stage.path().join("journal");
    write_new(&journal_stage, &json_bytes(&j)?)?;
    fs::rename(&journal_stage, root.join(JOURNAL)).map_err(err)?;
    // Do not auto-delete backup evidence if interrupted or rollback itself fails.
    let _owned = stage.keep();
    let result = (|| {
        for (i, (target, new)) in changes.iter().enumerate() {
            before(i * 2)?;
            let path = root.join(target);
            if j.changes[i].old.is_some() {
                fs::rename(&path, root.join(&j.directory).join(format!("old-{i}"))).map_err(err)?;
            }
            before(i * 2 + 1)?;
            if let Some(new) = new {
                fs::rename(new, &path).map_err(err)?;
            }
        }
        before(changes.len() * 2)?;
        Ok(())
    })();
    if let Err(error) = result {
        return match rollback(root, &j) {
            Ok(()) => Err(error),
            Err(r) => Err(format!(
                "{error}; rollback incomplete: {r}; run resource recover"
            )),
        };
    }
    // Commit point: removing the journal makes installed bytes authoritative.
    // Orphan staging after this point contains only old bytes, never active data.
    if let Err(error) = fs::remove_file(root.join(JOURNAL)) {
        return match rollback(root,&j) {
            Ok(())=>Err(format!("commit marker removal failed: {error}; rolled back")),
            Err(rollback)=>Err(format!("commit marker removal failed: {error}; rollback incomplete: {rollback}; run resource recover")),
        };
    }
    if let Err(e) = fs::remove_dir_all(root.join(&j.directory)) {
        return Ok(vec![format!(
            "committed; backup cleanup pending (OS error {:?}, {:?})",
            e.raw_os_error(),
            e.kind()
        )]);
    }
    Ok(Vec::new())
}
fn stage_dir(root: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(&format!(".resource-txn-{}", uuid::Uuid::new_v4().simple()))
        .rand_bytes(0)
        .tempdir_in(root)
        .map_err(err)
}

#[derive(Default)]
struct Options {
    op: String,
    kind: Option<Kind>,
    name: Option<String>,
    scope: Option<Scope>,
    source: Option<PathBuf>,
    version: Option<String>,
}
fn parse(args: &[String]) -> Result<Options> {
    let mut o = Options {
        op: args.first().ok_or("missing resource operation")?.clone(),
        ..Default::default()
    };
    let mut pos = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if matches!(a.as_str(), "--scope" | "--path" | "--version") {
            i += 1;
            let v = args.get(i).ok_or("missing option value")?;
            if v.starts_with('-') {
                return Err("missing option value".into());
            }
            match a.as_str() {
                "--scope" => {
                    if o.scope.is_some() {
                        return Err("duplicate scope".into());
                    }
                    o.scope = Some(match v.as_str() {
                        "project" => Scope::Project,
                        "global" => Scope::Global,
                        _ => return Err("scope must be project or global".into()),
                    });
                }
                "--path" => {
                    if o.source.replace(v.into()).is_some() {
                        return Err("duplicate path".into());
                    }
                }
                _ => {
                    if v.trim().is_empty()
                        || v.len() > 128
                        || v.chars().any(char::is_control)
                        || o.version.replace(v.clone()).is_some()
                    {
                        return Err("invalid/duplicate version".into());
                    }
                }
            }
        } else if a.starts_with('-') {
            return Err(format!(
                "unknown resource option {a}; builds require explicit native package link/install"
            ));
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    if o.scope.is_none() {
        return Err("explicit --scope project|global is required".into());
    }
    if matches!(o.op.as_str(), "list" | "recover") {
        if !pos.is_empty() || o.source.is_some() || o.version.is_some() {
            return Err("unexpected list/recover arguments".into());
        }
    } else if matches!(
        o.op.as_str(),
        "inspect" | "install" | "update" | "reinstall" | "remove"
    ) {
        if pos.len() != 2 {
            return Err("expected KIND NAME".into());
        }
        o.kind = Some(Kind::parse(&pos[0])?);
        if o.op == "inspect" && o.kind == Some(Kind::Package) {
            legacy_package_identity(&pos[1])?;
        } else {
            identifier(&pos[1])?;
        }
        o.name = Some(pos[1].clone());
        let mutation = matches!(o.op.as_str(), "install" | "update" | "reinstall");
        if mutation != o.source.is_some() || (!mutation && o.version.is_some()) {
            return Err("install/update/reinstall require --path; inspect/remove reject it".into());
        }
    } else {
        return Err("unknown resource operation".into());
    }
    Ok(o)
}
fn validate_source(kind: Kind, source: &Path, inv: &Inventory) -> Result<()> {
    match kind {
        Kind::Skill => {
            if !source.is_dir() || !source.join("SKILL.md").is_file() {
                return Err("standalone Skill requires a directory containing SKILL.md".into());
            }
            let text =
                read_bytes(&source.join("SKILL.md"), 64 * 1024)?.ok_or("missing SKILL.md")?;
            std::str::from_utf8(&text).map_err(err)?;
        }
        Kind::Extension => {
            if !source.is_file()
                || source.extension().and_then(|s| s.to_str())
                    != Some(std::env::consts::DLL_EXTENSION)
            {
                return Err("extension source must be a platform native library file".into());
            }
        }
        Kind::Package => {
            if !source.is_dir() {
                return Err("package source must be a directory".into());
            }
            if !["extensions", "skills", "prompts", "themes"]
                .iter()
                .any(|s| source.join(s).is_dir())
                && inv.files.is_empty()
            {
                return Err("empty package".into());
            }
            for f in &inv.files {
                if f.path.starts_with("extensions")
                    && (f.path.components().count() != 2
                        || f.path.extension().and_then(|s| s.to_str())
                            != Some(std::env::consts::DLL_EXTENSION))
                {
                    return Err(
                        "Package extensions must be direct platform native artifacts".into(),
                    );
                }
            }
        }
    }
    Ok(())
}
fn explicit_source(path: &Path) -> Result<PathBuf> {
    let addressed = std::env::current_dir().map_err(err)?.join(path);
    let mut normalized = PathBuf::new();
    for component in addressed.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                checked_path(&normalized)?;
                if !normalized.pop() {
                    return Err("source escapes filesystem root".into());
                }
            }
            other => {
                normalized.push(other);
                // A Windows drive prefix (`C:` / `\\\\?\\C:`) becomes
                // absolute only after its RootDir component is appended.
                if !matches!(other, Component::Prefix(_)) && normalized.is_absolute() {
                    checked_path(&normalized)?;
                }
            }
        }
    }
    fs::canonicalize(normalized).map_err(err)
}
fn mutate(root: &Path, o: &Options) -> Result<Value> {
    let _lease = mutation_lease(root)?;
    let mut r = receipts(root)?;
    let mut p = packages(root)?;
    let native: Vec<crate::install::InstalledNativePackage> =
        read_json(&root.join("native-packages.json"))?;
    let kind = o.kind.ok_or("missing kind")?;
    let name = o.name.as_deref().ok_or("missing name")?;
    let target = kind.target(name);
    let path = root.join(&target);
    checked_path(&path)?;
    let previous = r
        .resources
        .iter()
        .find(|r| r.kind == kind && r.name.eq_ignore_ascii_case(name))
        .cloned();
    for owned in r.resources.iter().filter(|r| r.kind == Kind::Package) {
        if p.packages.iter().find(|p| p.name == owned.name) != Some(&package_record(owned)) {
            return Err(
                "owned package receipt disagrees with native packages.json; refusing mutation"
                    .into(),
            );
        }
    }
    if kind == Kind::Extension
        && native.iter().any(|p| {
            p.name.eq_ignore_ascii_case(name)
                || p.artifacts.iter().any(|a| {
                    target
                        .file_name()
                        .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(a))
                })
        })
    {
        return Err(
            "extension identity/artifact owned by legacy native registry (even if missing)".into(),
        );
    }
    if let Some(old) = &previous {
        if old.name != name {
            return Err("identity case collision".into());
        }
        if inventory(&path)? != old.inventory {
            return Err("owned resource content changed; refusing overwrite/removal".into());
        }
    }
    if o.op == "remove" && previous.is_none() {
        if path.try_exists().map_err(err)? {
            return Err("unmanaged destination: refusing removal".into());
        }
        return Ok(json!({"outcome":"absent","kind":kind,"name":name}));
    }
    if previous.is_none() && path.try_exists().map_err(err)? {
        return Err("unmanaged destination conflict".into());
    }
    if kind == Kind::Package
        && previous.is_none()
        && p.packages.iter().any(|p| p.name.eq_ignore_ascii_case(name))
    {
        return Err("package identity owned by native package link".into());
    }
    if matches!(o.op.as_str(), "update" | "reinstall") && previous.is_none() {
        return Err("resource is not owned/installed; use install".into());
    }
    let source = o
        .source
        .as_ref()
        .map(|path| explicit_source(path))
        .transpose()?;
    if let Some(source) = &source {
        let source_identity = path_identity(source)?;
        let root_identity = path_identity(root)?;
        if source_identity.starts_with(&root_identity)
            || root_identity.starts_with(&source_identity)
        {
            return Err("source/destination resource trees overlap".into());
        }
    }
    let stage = stage_dir(root)?;
    let mut next = None;
    if let Some(source) = source {
        // No Cargo metadata/build or native loader is called here.
        let inv = inventory(&source)?;
        validate_source(kind, &source, &inv)?;
        let version = o
            .version
            .clone()
            .or_else(|| previous.as_ref().and_then(|r| r.version.clone()));
        let receipt = Receipt {
            kind,
            name: name.into(),
            source,
            version,
            target: target.clone(),
            inventory: inv,
        };
        if o.op == "install" {
            if let Some(old) = &previous {
                if old.source == receipt.source
                    && old.version == receipt.version
                    && old.inventory == receipt.inventory
                {
                    return Ok(
                        json!({"outcome":"unchanged","resource":observe(root,o.scope.unwrap(),old,false)?}),
                    );
                }
                return Err("already installed; use update or reinstall with --path".into());
            }
        }
        let staged = stage.path().join("snapshot");
        copy_snapshot(&receipt.source, &staged, &receipt.inventory)?;
        next = Some((receipt, staged));
    }
    r.resources.retain(|r| !(r.kind == kind && r.name == name));
    if kind == Kind::Package {
        p.packages.retain(|p| p.name != name);
        if let Some((new, _)) = &next {
            p.packages.push(package_record(new));
        }
    }
    if let Some((receipt, _)) = &next {
        r.resources.push(receipt.clone());
    }
    if r.resources.len() > 128 {
        return Err("resource receipts exceed 128 resources per scope".into());
    }
    r.resources.sort_by_key(|r| r.target.clone());
    p.packages.sort_by(|a, b| a.name.cmp(&b.name));
    let receipt_stage = stage.path().join("receipts");
    write_new(&receipt_stage, &json_bytes(&r)?)?;
    let mut changes = vec![
        (target, next.as_ref().map(|(_, p)| p.clone())),
        (PathBuf::from(RECEIPTS), Some(receipt_stage)),
    ];
    if kind == Kind::Package {
        let staged = stage.path().join("packages");
        write_new(&staged, &json_bytes(&p)?)?;
        changes.push((PathBuf::from("packages.json"), Some(staged)));
    }
    let mut data = json!({"outcome":if o.op=="remove"{"removed"}else if previous.is_some(){"replaced"}else{"installed"},"kind":kind,"name":name,"diagnostics":[],"resource":next.as_ref().map(|(r,_)|observe_inventory(root,o.scope.unwrap(),r,false,Ok(r.inventory.clone()))).transpose()?});
    // Bound the exact public envelope before publication. Reserve space for a
    // bounded post-commit backup-cleanup diagnostic (not a false operation error).
    if json_bytes(&success(o, data.clone()))
        .map_err(|error| format!("resource success result rejected before publication: {error}"))?
        .len()
        > MAX_META - 4096
    {
        return Err("resource success result exceeds bounded publication budget".into());
    }
    let parent = path.parent().ok_or("no resource parent")?;
    checked_path(parent)?;
    fs::create_dir_all(parent).map_err(err)?;
    let diagnostics = publish(root, changes, stage, |_| Ok(()))?;
    data["diagnostics"] = json!(diagnostics);
    Ok(data)
}
fn package_record(r: &Receipt) -> crate::packages::ProjectPackage {
    crate::packages::ProjectPackage {
        name: r.name.clone(),
        version: r.version.clone().unwrap_or_default(),
        source: r.source.to_string_lossy().into_owned(),
        cargo_package: String::new(),
        artifacts: r
            .inventory
            .files
            .iter()
            .filter(|f| f.path.starts_with("extensions"))
            .filter_map(|f| {
                f.path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_owned)
            })
            .collect(),
    }
}
fn observe(root: &Path, scope: Scope, r: &Receipt, busy: bool) -> Result<Value> {
    observe_bounded(root, scope, r, busy, &mut InventoryBudget::default())
}
fn observe_bounded(
    root: &Path,
    scope: Scope,
    r: &Receipt,
    busy: bool,
    budget: &mut InventoryBudget,
) -> Result<Value> {
    let observed = inventory_bounded(&root.join(&r.target), budget);
    observe_inventory(root, scope, r, busy, observed)
}
fn observe_inventory(
    root: &Path,
    scope: Scope,
    r: &Receipt,
    busy: bool,
    observed: Result<Inventory>,
) -> Result<Value> {
    let mut diagnostics =
        vec!["native ABI compatibility is unverified; inspection never loads code".to_string()];
    if observed.as_ref().ok() != Some(&r.inventory) {
        diagnostics.push("installed content is missing or differs from ownership receipt".into());
    }
    if let Err(e) = &observed {
        diagnostics.push(e.clone());
    }
    Ok(
        json!({"kind":r.kind,"scope":scope,"identity":format!("{}:{}:{}",if scope==Scope::Project{"project"}else{"global"},r.kind.dir(),r.name),"name":r.name,"source":r.source,"version":r.version,"ownership":"resource","installed":true,"inUse":busy,"loaded":null,"applied":null,"path":root.join(&r.target),"selectable":selectable(root,scope,r),"recordedDigest":r.inventory.sha256,"observed":observed.ok(),"diagnostics":diagnostics}),
    )
}
fn selectable(root: &Path, scope: Scope, r: &Receipt) -> Value {
    match r.kind {
        Kind::Package => json!({"main_package":{"name":r.name,"scope":scope,"instructions":[]}}),
        Kind::Skill => json!({"skills":[root.join(&r.target)]}),
        Kind::Extension => json!({"extensions":[root.join(&r.target)]}),
    }
}
fn catalog(root: &Path, o: &Options) -> Result<Value> {
    checked_path(root)?;
    let (_observation_lease, busy) = observation_lease(root)?;
    let r = receipts(root)?;
    let p = packages(root)?;
    let native: Vec<crate::install::InstalledNativePackage> =
        read_json(&root.join("native-packages.json"))?;
    let scope = o.scope.unwrap();
    let mut items = Vec::new();
    let mut owned = BTreeSet::new();
    let mut budget = InventoryBudget::default();
    for receipt in &r.resources {
        owned.insert(receipt.target.clone());
        items.push(observe_bounded(root, scope, receipt, busy, &mut budget)?);
    }
    for kind in [Kind::Package, Kind::Skill, Kind::Extension] {
        let directory = root.join(kind.dir());
        checked_path(&directory)?;
        let entries = match fs::read_dir(directory) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(err(e)),
        };
        for entry in entries {
            let entry = entry.map_err(err)?;
            let target = Path::new(kind.dir()).join(entry.file_name());
            if owned.contains(&target) {
                continue;
            }
            if items.len() >= MAX_FILES {
                return Err("catalog exceeds entry limit".into());
            }
            let name = if kind == Kind::Extension {
                entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                entry
                    .file_name()
                    .to_str()
                    .ok_or("non UTF8 candidate")?
                    .to_string()
            };
            let pkg = p
                .packages
                .iter()
                .find(|p| kind == Kind::Package && p.name == name);
            let ext = native.iter().find(|p| {
                kind == Kind::Extension
                    && p.artifacts
                        .iter()
                        .any(|a| entry.file_name() == std::ffi::OsStr::new(a))
            });
            let inv = inventory_bounded(&entry.path(), &mut budget);
            let diagnostics = match &inv {
                Ok(_) => vec!["unverified candidate; no code executed".to_string()],
                Err(e) => vec![e.clone()],
            };
            items.push(json!({"kind":kind,"scope":scope,"identity":format!("{}:{}:{}",if scope==Scope::Project{"project"}else{"global"},kind.dir(),name),"name":name,"source":pkg.map(|p|p.source.clone()).or_else(||ext.and_then(|p|p.source.clone())),"version":pkg.and_then(|p|if p.version.is_empty(){None}else{Some(&p.version)}).or_else(||ext.filter(|p|!p.version.trim().is_empty()).map(|p|&p.version)),"ownership":if pkg.is_some()||ext.is_some(){"legacy-native"}else{"unmanaged"},"installed":pkg.is_some()||ext.is_some(),"inUse":busy,"loaded":null,"applied":null,"path":entry.path(),"observed":inv.ok(),"diagnostics":diagnostics}));
        }
    }
    if o.op == "inspect" {
        items.retain(|item| item["name"] == json!(o.name) && item["kind"] == json!(o.kind));
        if items.is_empty() {
            return Err("resource not found".into());
        }
    }
    items.sort_by_key(|i| i["identity"].as_str().unwrap_or("").to_string());
    // When no persistent lock existed initially, first publication could have
    // started during traversal. Its lock inode is never unlinked: reject and
    // retry rather than returning any potentially mixed observation.
    if _observation_lease.is_none() && lease_path(root).try_exists().map_err(err)? {
        return Err("resource ownership initialized during observation; retry catalog".into());
    }
    Ok(
        json!({"outcome":"observed","resources":items,"diagnostics":if root.join(JOURNAL).try_exists().map_err(err)?{vec!["incomplete transaction; run resource recover"]}else{vec![]}}),
    )
}
fn success(o: &Options, data: Value) -> Value {
    json!({"version":1,"success":true,"operation":o.op,"scope":o.scope,"kind":o.kind,"name":o.name,"data":data})
}
pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| matches!(a.as_str(), "--help" | "-h")) {
        println!("rpi resource list|inspect|install|update|reinstall|remove|recover --scope project|global\nMutations: KIND NAME --path PATH [--version TEXT]; remove/inspect: KIND NAME.\nLocal snapshots only; no builds or native registration. JSON v1, 256 KiB bound.");
        return 0;
    }
    let result: Result<Value> = (|| {
        let o = parse(args)?;
        let cwd = std::env::current_dir().map_err(err)?;
        let root = o.scope.unwrap().root(&cwd)?;
        let data = if matches!(o.op.as_str(), "list" | "inspect") {
            catalog(&root, &o)?
        } else if o.op == "recover" {
            let _lease = Lease::acquire(&root, true, true)?;
            json!({"outcome":if recover(&root)?{"recovered"}else{"unchanged"}})
        } else {
            mutate(&root, &o)?
        };
        Ok(success(&o, data))
    })();
    match result.and_then(|v| json_bytes(&v)) {
        Ok(bytes) => {
            println!("{}", String::from_utf8_lossy(&bytes));
            0
        }
        Err(e) => {
            let bounded = e.chars().take(2048).collect::<String>();
            let context = parse(args).ok();
            println!(
                "{}",
                json!({"version":1,"success":false,"operation":context.as_ref().map(|o|&o.op),"scope":context.as_ref().and_then(|o|o.scope),"kind":context.as_ref().and_then(|o|o.kind),"name":context.as_ref().and_then(|o|o.name.as_ref()),"error":bounded})
            );
            1
        }
    }
}

#[cfg(test)]
mod regressions;
#[cfg(test)]
mod tests;
