use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use gpui_kit::Global;
use k8s_core::update::{
    UPDATE_TARGET_ARCH, UPDATE_TARGET_OS, UpdateArtifact, UpdateManifest, UpdatePhase,
    controlled_version_dir, is_newer_version, parse_and_validate_manifest, parse_version,
    read_update_state, validate_manifest, write_update_state,
};
use reqwest::Url;
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub use k8s_core::update::UpdateState as UpdaterStatus;

#[cfg(target_arch = "x86_64")]
const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/0x5457/k8s-gpui/releases/latest/download/update-linux-x86_64.json";
#[cfg(target_arch = "aarch64")]
const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/0x5457/k8s-gpui/releases/latest/download/update-linux-aarch64.json";
const MANIFEST_URL: &str = match option_env!("K8S_GPUI_UPDATE_MANIFEST_URL") {
    Some(value) => value,
    None => DEFAULT_MANIFEST_URL,
};
const PUBLIC_KEY_HEX: Option<&str> = option_env!("K8S_GPUI_UPDATE_PUBLIC_KEY");
const CHANNEL: &str = "stable";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The one binary an installation and a release contain.
const APP_BINARY_NAME: &str = "k8s-app";
/// The release asset the updater downloads for this build's architecture.
/// Both Linux architectures ship in one release, so asset names carry the
/// arch suffix; the tarball keeps the plain binary name inside instead.
fn app_asset_name() -> String {
    format!("{APP_BINARY_NAME}-linux-{UPDATE_TARGET_ARCH}")
}
const POLL_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_BASE: Duration = Duration::from_secs(15 * 60);
const MAX_MANIFEST_SIZE: usize = 1024 * 1024;
const MAX_SIGNATURE_SIZE: usize = 64;
const MAX_ARTIFACT_SIZE: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ETAG_SIZE: usize = 4096;
const MAX_RETAINED_INSTALLATIONS: usize = 3;
const MAX_RETAINED_CACHE_ENTRIES: usize = 3;
const MAX_INSTALLATION_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_CACHE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const ACTIVE_PART_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const STALE_TEMP_DIR_AGE: Duration = Duration::from_secs(60 * 60);
const UPDATE_LOCK_FILE: &str = ".updater.lock";
const TEMP_DIR_PREFIXES: [&str; 4] = [".staging-", ".install-user-", ".current-", ".write-check-"];
const UPDATE_DISABLE_ENV: &str = "K8S_GPUI_DISABLE_UPDATES";
const MANAGED_INSTALL_ACTION: &str = "Run `k8s-app install-user`, then start the installed app.";

fn updates_disabled_by_environment() -> bool {
    std::env::var_os(UPDATE_DISABLE_ENV).is_some()
}

fn updater_configured_for(
    supported_platform: bool,
    debug_build: bool,
    public_key: bool,
    updates_disabled: bool,
) -> bool {
    supported_platform && !debug_build && public_key && !updates_disabled
}

pub fn updater_configured() -> bool {
    updater_configured_for(
        cfg!(target_os = "linux"),
        cfg!(debug_assertions),
        PUBLIC_KEY_HEX.is_some(),
        updates_disabled_by_environment(),
    )
}

pub fn initial_unavailable_reason() -> Option<String> {
    if let Err(error) = recover_installations() {
        eprintln!("k8s-gpui: installation recovery failed: {error:#}");
    }
    if !updater_configured() {
        return Some(updater_unavailable_reason());
    }
    let version = match parse_version(env!("CARGO_PKG_VERSION")) {
        Ok(version) => version,
        Err(error) => return Some(format!("Compiled version is invalid: {error}")),
    };
    managed_installation(&controlled_version_dir(&version)).err()
}

fn updater_unavailable_reason() -> String {
    if updates_disabled_by_environment() {
        "Automatic updates are disabled by K8S_GPUI_DISABLE_UPDATES. Remove the variable and restart the app.".to_owned()
    } else if !cfg!(target_os = "linux") {
        "Automatic updates are not available on this platform. Use Linux or update through the package or source that installed the app."
            .to_owned()
    } else if cfg!(debug_assertions) {
        "Automatic updates are not available in debug builds. Run a release build to enable update checks and installation."
            .to_owned()
    } else {
        "Automatic updates are not available because this build has no signing key. Install a signed release build."
            .to_owned()
    }
}

pub type UpdaterStatusCallback = Arc<dyn Fn(UpdaterStatus) + Send + Sync>;

#[derive(Clone)]
pub struct GlobalUpdater(pub UpdaterRuntime);

impl Global for GlobalUpdater {}

struct RuntimeInner {
    client: reqwest::Client,
    status: UpdaterStatusCallback,
    current_version: String,
    current_version_dir: String,
    state: Mutex<UpdaterStatus>,
    operation: AsyncMutex<()>,
}

#[derive(Clone)]
pub struct UpdaterRuntime {
    inner: Arc<RuntimeInner>,
}

#[derive(Clone, Debug)]
struct FetchedManifest {
    manifest: UpdateManifest,
    etag: Option<String>,
}

#[derive(Clone, Debug)]
struct BinaryArtifacts {
    app: UpdateArtifact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstallPlan {
    root: PathBuf,
    current: PathBuf,
    version_dir: PathBuf,
    version_dir_name: String,
}

impl InstallPlan {
    fn new(root: PathBuf, version_dir_name: String) -> Result<Self> {
        ensure!(
            !version_dir_name.is_empty()
                && version_dir_name != "."
                && version_dir_name != ".."
                && !version_dir_name.contains('/')
                && !version_dir_name.contains('\\'),
            "Installation version directory name is invalid."
        );
        let version_dir = root.join(&version_dir_name);
        let current = root.join("current");
        Ok(Self {
            root,
            current,
            version_dir,
            version_dir_name,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingVersionAction {
    Stage,
    Reuse,
    Remove,
    PreserveCurrent,
}

fn existing_version_action(exists: bool, current: bool, complete: bool) -> ExistingVersionAction {
    if !exists {
        ExistingVersionAction::Stage
    } else if current {
        if complete {
            ExistingVersionAction::Reuse
        } else {
            ExistingVersionAction::PreserveCurrent
        }
    } else if complete {
        ExistingVersionAction::Reuse
    } else {
        ExistingVersionAction::Remove
    }
}

#[derive(Clone, Debug)]
struct CleanupEntry {
    name: String,
    path: PathBuf,
    modified: Option<SystemTime>,
    installed: bool,
}

fn is_version_dir_name(name: &str) -> bool {
    name.strip_prefix('v')
        .is_some_and(|version| parse_version(version).is_ok())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {}.", path.display())),
    }
}

struct UpdateLock {
    _file: File,
}

fn update_lock_parent(root: &Path) -> &Path {
    root.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(root)
}

fn open_update_lock(root: &Path) -> io::Result<File> {
    let parent = update_lock_parent(root);
    k8s_core::atomic_file::create_private_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(parent.join(UPDATE_LOCK_FILE))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn acquire_update_lock(root: &Path) -> Result<UpdateLock> {
    let file = open_update_lock(root)
        .with_context(|| format!("Failed to open the updater lock for {}.", root.display()))?;
    file.lock()
        .with_context(|| format!("Failed to lock updater state for {}.", root.display()))?;
    Ok(UpdateLock { _file: file })
}

#[cfg(test)]
fn try_acquire_update_lock(root: &Path) -> Result<UpdateLock> {
    let file = open_update_lock(root)
        .with_context(|| format!("Failed to open the updater lock for {}.", root.display()))?;
    file.try_lock()
        .with_context(|| format!("Failed to lock updater state for {}.", root.display()))?;
    Ok(UpdateLock { _file: file })
}

fn current_installation_name(current: &Path) -> Option<String> {
    fs::read_link(current)
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}

fn installation_is_current(plan: &InstallPlan, path: &Path) -> bool {
    path_is_current_link(&plan.current, path)
}

fn path_is_current_link(current: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if let Ok(target) = fs::read_link(current)
        && target.file_name().and_then(|target| target.to_str()) == Some(name)
    {
        return true;
    }
    match (fs::canonicalize(current), fs::canonicalize(path)) {
        (Ok(current), Ok(path)) => current == path,
        _ => false,
    }
}

fn running_installation_name(root: &Path) -> Option<String> {
    let executable = fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let parent = executable.parent()?;
    let parent_is_root = parent == root
        || fs::canonicalize(parent)
            .map(|path| path == root)
            .unwrap_or(false);
    if !parent_is_root {
        return None;
    }
    parent
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn protected_installation_names(
    current: &Path,
    active: Option<&str>,
    running: Option<&str>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(current) = current_installation_name(current) {
        names.insert(current);
    }
    if let Some(active) = active {
        names.insert(active.to_owned());
    }
    if let Some(running) = running {
        names.insert(running.to_owned());
    }
    names
}

fn remove_installation_entry(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn installation_looks_installed(path: &Path) -> bool {
    installation_files_are_valid(path).unwrap_or(false)
}

fn installation_is_complete(path: &Path, artifacts: &BinaryArtifacts) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}.", path.display()));
        }
    };
    if !metadata.file_type().is_dir() {
        return Ok(false);
    }
    let app_hash = decode_sha256(&artifacts.app.sha256)?;
    Ok(verify_binary_file(&path.join("k8s-app"), artifacts.app.size, &app_hash).is_ok())
}

fn installation_files_are_valid(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}.", path.display()));
        }
    };
    if !metadata.file_type().is_dir() {
        return Ok(false);
    }
    let path = path.join(APP_BINARY_NAME);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}.", path.display()));
        }
    };
    if !metadata.file_type().is_file() || ensure_elf_binary(&path).is_err() {
        return Ok(false);
    }
    Ok(true)
}

fn recover_user_installation(plan: &InstallPlan) -> Result<Option<PathBuf>> {
    let exists = path_exists(&plan.version_dir)?;
    let current = installation_is_current(plan, &plan.version_dir)
        || running_installation_name(&plan.root).as_deref() == Some(plan.version_dir_name.as_str());
    let complete = if exists {
        installation_files_are_valid(&plan.version_dir)?
    } else {
        false
    };
    match existing_version_action(exists, current, complete) {
        ExistingVersionAction::Stage => Ok(None),
        ExistingVersionAction::Reuse => {
            switch_current(plan)?;
            Ok(Some(plan.version_dir.join("k8s-app")))
        }
        ExistingVersionAction::Remove => {
            remove_installation_entry(&plan.version_dir).with_context(|| {
                format!(
                    "Failed to remove incomplete installation {}.",
                    plan.version_dir.display()
                )
            })?;
            Ok(None)
        }
        ExistingVersionAction::PreserveCurrent => {
            bail!(
                "The current managed installation is incomplete and was not removed. Run `k8s-app install-user` again."
            )
        }
    }
}

fn recover_user_installation_with_cleanup(
    plan: &InstallPlan,
    staging: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let _lock = acquire_update_lock(&plan.root)?;
    let executable = recover_user_installation(plan)?;
    if executable.is_some() {
        cleanup_after_switch(plan, staging);
    }
    Ok(executable)
}

fn activate_user_installation(
    plan: &InstallPlan,
    staging: &Path,
    staging_guard: &mut DirectoryGuard,
) -> Result<PathBuf> {
    if let Some(executable) = recover_user_installation(plan)? {
        cleanup_after_switch(plan, Some(staging));
        return Ok(executable);
    }
    if let Err(first) = activate_staged_version(staging, plan) {
        if let Some(executable) = recover_user_installation(plan)? {
            cleanup_after_switch(plan, Some(staging));
            return Ok(executable);
        }
        activate_staged_version(staging, plan).map_err(|second| {
            anyhow!(
                "Failed to activate the managed version directory: {second}. The first attempt failed: {first}."
            )
        })?;
    }
    staging_guard.disarm();
    let mut installed_guard = DirectoryGuard::managed(plan);
    switch_current(plan)?;
    installed_guard.disarm();
    cleanup_after_switch(plan, Some(staging));
    Ok(plan.version_dir.join("k8s-app"))
}

fn cleanup_should_remove(
    retained_count: usize,
    age: Option<Duration>,
    max_entries: usize,
    max_age: Duration,
) -> bool {
    retained_count > max_entries || age.is_some_and(|age| age >= max_age)
}

fn entry_age(now: SystemTime, modified: Option<SystemTime>) -> Option<Duration> {
    modified.and_then(|modified| now.duration_since(modified).ok())
}

fn collect_cleanup_entries(root: &Path) -> Vec<CleanupEntry> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            if !is_version_dir_name(&name) {
                return None;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_dir() {
                return None;
            }
            Some(CleanupEntry {
                installed: installation_looks_installed(&path),
                name,
                path,
                modified: metadata.modified().ok(),
            })
        })
        .collect()
}

fn sort_cleanup_entries(entries: &mut [CleanupEntry]) {
    entries.sort_by(|left, right| match (left.modified, right.modified) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.name.cmp(&right.name),
    });
}

fn cleanup_installation_dirs(root: &Path, protected: &HashSet<String>, now: SystemTime) {
    let mut entries = collect_cleanup_entries(root);
    sort_cleanup_entries(&mut entries);
    let mut retained = entries
        .iter()
        .filter(|entry| protected.contains(&entry.name))
        .count();
    for entry in entries {
        if protected.contains(&entry.name) {
            continue;
        }
        let removable = !entry.installed
            || cleanup_should_remove(
                retained.saturating_add(1),
                entry_age(now, entry.modified),
                MAX_RETAINED_INSTALLATIONS,
                MAX_INSTALLATION_AGE,
            );
        if removable {
            if remove_installation_entry(&entry.path).is_err() {
                retained += 1;
            }
        } else {
            retained += 1;
        }
    }
}

fn cache_entry_has_active_part(path: &Path, now: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return true;
    };
    entries.filter_map(Result::ok).any(|entry| {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".part"))
        {
            return false;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        match entry_age(now, modified) {
            Some(age) => age < ACTIVE_PART_MAX_AGE,
            None => true,
        }
    })
}

fn cleanup_cache_dirs(root: &Path, protected: &HashSet<String>, now: SystemTime) {
    let mut entries = collect_cleanup_entries(root);
    sort_cleanup_entries(&mut entries);
    let mut retained = entries
        .iter()
        .filter(|entry| {
            protected.contains(&entry.name) || cache_entry_has_active_part(&entry.path, now)
        })
        .count();
    for entry in entries {
        if protected.contains(&entry.name) || cache_entry_has_active_part(&entry.path, now) {
            continue;
        }
        let removable = cleanup_should_remove(
            retained.saturating_add(1),
            entry_age(now, entry.modified),
            MAX_RETAINED_CACHE_ENTRIES,
            MAX_CACHE_AGE,
        );
        if removable {
            if remove_installation_entry(&entry.path).is_err() {
                retained += 1;
            }
        } else {
            retained += 1;
        }
    }
}

fn is_temp_dir_name(name: &str) -> bool {
    TEMP_DIR_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn remove_stale_temp_dirs(root: &Path, keep: Option<&Path>, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_temp_dir_name(name) {
            continue;
        }
        let path = entry.path();
        if keep == Some(path.as_path()) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        match entry_age(now, metadata.modified().ok()) {
            Some(age) if age >= STALE_TEMP_DIR_AGE => {}
            Some(_) | None => continue,
        }
        let _ = remove_installation_entry(&path);
    }
}

fn cleanup_after_switch(plan: &InstallPlan, staging: Option<&Path>) {
    let running = running_installation_name(&plan.root);
    let active = Some(plan.version_dir_name.as_str());
    let protected = protected_installation_names(&plan.current, active, running.as_deref());
    let now = SystemTime::now();
    remove_stale_temp_dirs(&plan.root, staging, now);
    cleanup_installation_dirs(&plan.root, &protected, now);
    if let Ok(cache_dir) = update_cache_dir() {
        cleanup_cache_dirs(&cache_dir, &protected, now);
    }
}

fn recover_installations_in(root: &Path, running: Option<&str>, cache: Option<&Path>) {
    let now = SystemTime::now();
    let protected = protected_installation_names(&root.join("current"), None, running);
    remove_stale_temp_dirs(root, None, now);
    cleanup_installation_dirs(root, &protected, now);
    if let Some(cache) = cache {
        cleanup_cache_dirs(cache, &protected, now);
    }
}

fn recover_installations() -> Result<()> {
    let data_dir =
        k8s_core::paths::data_dir().context("The application data directory is unavailable.")?;
    let root = data_dir.join("installations");
    if !path_exists(&root)? {
        return Ok(());
    }
    let root = fs::canonicalize(&root).with_context(|| {
        format!(
            "The managed installation directory was not readable during recovery. Check the app data directory, then restart the app. Detail: {}",
            root.display()
        )
    })?;
    let _lock = acquire_update_lock(&root)?;
    let running = running_installation_name(&root);
    let cache = update_cache_dir().ok();
    recover_installations_in(&root, running.as_deref(), cache.as_deref());
    Ok(())
}

pub fn install_user() -> Result<PathBuf> {
    ensure!(
        cfg!(target_os = "linux"),
        "User installation is not available on this platform. Run `k8s-app install-user` from a Linux release build."
    );
    ensure!(
        !cfg!(debug_assertions),
        "Debug builds cannot create a managed installation. Run `k8s-app install-user` from a release build."
    );
    let source_app = fs::canonicalize(std::env::current_exe()?)
        .context("Failed to resolve the current executable.")?;
    ensure!(
        source_app.file_name().and_then(|name| name.to_str()) == Some("k8s-app"),
        "The current executable is not `k8s-app`. Run `k8s-app install-user` from the `k8s-app` binary."
    );
    let version = parse_version(env!("CARGO_PKG_VERSION"))?;
    let data_dir =
        k8s_core::paths::data_dir().context("The application data directory is unavailable.")?;
    let root = data_dir.join("installations");
    k8s_core::atomic_file::create_private_dir_all(&root)
        .context("Failed to create the managed installation directory.")?;
    let root =
        fs::canonicalize(&root).context("Failed to resolve the managed installation directory.")?;
    let version_dir_name = controlled_version_dir(&version);
    let plan = InstallPlan::new(root.clone(), version_dir_name)?;
    if let Some(executable) = recover_user_installation_with_cleanup(&plan, None)? {
        install_user_launchers(&data_dir)?;
        return Ok(executable);
    }
    let staging = Builder::new()
        .prefix(".install-user-")
        .tempdir_in(&root)
        .context("Failed to create the user installation staging directory.")?;
    let app_destination = staging.path().join("k8s-app");
    copy_user_binary(&source_app, &app_destination)?;
    let staging_path = staging.keep();
    let mut staging_guard = DirectoryGuard::new(staging_path.clone());
    if let Some(executable) =
        recover_user_installation_with_cleanup(&plan, Some(staging_path.as_path()))?
    {
        drop(staging_guard);
        install_user_launchers(&data_dir)?;
        return Ok(executable);
    }
    let executable = {
        let _lock = acquire_update_lock(&plan.root)?;
        activate_user_installation(&plan, &staging_path, &mut staging_guard)
    }?;
    drop(staging_guard);
    install_user_launchers(&data_dir)?;
    Ok(executable)
}

fn copy_user_binary(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.file_type().is_file(),
        "User installation source is not a regular file."
    );
    fs::copy(source, destination).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    ensure_elf_binary(destination)?;
    File::open(destination)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn install_user_launchers(data_dir: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let home = k8s_core::paths::home_dir().context("The home directory is unavailable.")?;
    let bin_dir = home.join(".local").join("bin");
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("Failed to create {}", bin_dir.display()))?;
    let current = data_dir.join("current");
    let launcher = bin_dir.join(APP_BINARY_NAME);
    match fs::symlink_metadata(&launcher) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(&launcher)
                .with_context(|| format!("Failed to replace {}", launcher.display()))?;
        }
        Ok(_) => bail!(
            "{} already exists and is not a symlink.",
            launcher.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Failed to inspect the user launcher."),
    }
    symlink(current.join(APP_BINARY_NAME), &launcher)
        .with_context(|| format!("Failed to create {}", launcher.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn install_user_launchers(_data_dir: &Path) -> Result<()> {
    bail!("User launchers support only Unix.")
}

#[derive(Clone, Copy)]
struct InstallEnvironment {
    supported_platform: bool,
    debug_build: bool,
    flatpak: bool,
    app_image: bool,
}

impl InstallEnvironment {
    fn current() -> Self {
        Self {
            supported_platform: cfg!(target_os = "linux"),
            debug_build: cfg!(debug_assertions),
            flatpak: std::env::var_os("FLATPAK_ID").is_some()
                || std::env::var("container").is_ok_and(|value| value == "flatpak"),
            app_image: std::env::var_os("APPIMAGE").is_some()
                || std::env::var_os("APPDIR").is_some(),
        }
    }
}

struct PartGuard {
    path: PathBuf,
    armed: bool,
}

impl PartGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct DirectoryGuard {
    path: PathBuf,
    current: Option<PathBuf>,
    armed: bool,
}

impl DirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            current: None,
            armed: true,
        }
    }

    fn managed(plan: &InstallPlan) -> Self {
        Self {
            path: plan.version_dir.clone(),
            current: Some(plan.current.clone()),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if self.armed
            && !self
                .current
                .as_deref()
                .is_some_and(|current| path_is_current_link(current, &self.path))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl UpdaterRuntime {
    pub fn new(status: UpdaterStatusCallback) -> Result<Self> {
        let current_version = parse_version(env!("CARGO_PKG_VERSION"))
            .map_err(|error| anyhow!("Compiled version is invalid: {error}"))?;
        let current_version_dir = controlled_version_dir(&current_version);
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    attempt.error(
                        "The update server redirected to a non-HTTPS URL. Configure an HTTPS update server, then try again.",
                    )
                }
            }))
            .user_agent(format!(
                "k8s-gpui/{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_NAME")
            ))
            .build()
            .context("Failed to build the updater HTTP client.")?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                client,
                status,
                current_version: env!("CARGO_PKG_VERSION").to_owned(),
                current_version_dir,
                state: Mutex::new(UpdaterStatus::default()),
                operation: AsyncMutex::new(()),
            }),
        })
    }

    pub async fn check(&self) -> Result<()> {
        let _operation = self.inner.operation.lock().await;
        if !updater_configured() {
            self.set_unsupported(updater_unavailable_reason());
            return Ok(());
        }
        let current = self.status();
        if current.phase == UpdatePhase::Restarting
            || (current.phase == UpdatePhase::Ready && current.executable().is_some())
        {
            self.set_status(current);
            return Ok(());
        }
        let managed = match managed_installation(&self.inner.current_version_dir) {
            Ok(managed) => managed,
            Err(reason) => {
                self.set_unsupported(reason);
                return Ok(());
            }
        };

        self.set_status(UpdaterStatus::new(UpdatePhase::Checking));
        let fetched = match self.fetch_manifest().await {
            Ok(fetched) => fetched,
            Err(error) => return self.fail(error),
        };
        let manifest = fetched.manifest;
        if let Err(error) = self.validate_candidate(&manifest) {
            return self.fail(error);
        }
        let artifacts = match select_binary_artifacts(&manifest) {
            Ok(artifacts) => artifacts,
            Err(error) => return self.fail(error),
        };
        let current_version = match parse_version(&self.inner.current_version) {
            Ok(version) => version,
            Err(error) => return self.fail(anyhow!("Current version is invalid: {error}")),
        };
        if !is_newer_version(&current_version, &manifest.version) {
            let version = manifest.version.to_string();
            self.persist_success(fetched.etag, version.clone());
            self.set_status(UpdaterStatus::new(UpdatePhase::UpToDate).with_version(version));
            return Ok(());
        }

        let Some(artifacts) = artifacts else {
            self.persist_success(fetched.etag, manifest.version.to_string());
            self.set_unsupported(format!("No update is available for linux/{UPDATE_TARGET_ARCH}."));
            return Ok(());
        };

        let version = manifest.version.to_string();
        let version_dir = controlled_version_dir(&manifest.version);
        self.set_status(
            UpdaterStatus::new(UpdatePhase::Downloading)
                .with_version(version.clone())
                .with_progress(Some(0.0)),
        );
        let executable = match self
            .download_and_stage(&version, &version_dir, &artifacts, managed)
            .await
        {
            Ok(executable) => executable,
            Err(error) => return self.fail(error),
        };
        self.persist_success(fetched.etag, version.clone());
        self.set_status(
            UpdaterStatus::new(UpdatePhase::Ready)
                .with_version(version)
                .with_executable(executable),
        );
        Ok(())
    }

    pub fn request_restart(&self) -> Result<()> {
        let ready = self.status();
        let executable = ready
            .executable()
            .filter(|_| ready.phase == UpdatePhase::Ready)
            .map(Path::to_owned);
        let Some(executable) = executable else {
            let error = anyhow!("No staged update is ready. Check for updates first.");
            self.set_status(UpdaterStatus::new(UpdatePhase::Failed).with_error(error.to_string()));
            return Err(error);
        };
        self.set_status(
            UpdaterStatus::new(UpdatePhase::Restarting)
                .with_version(ready.version.unwrap_or_default())
                .with_executable(executable),
        );
        Ok(())
    }

    /// A check the person did not ask for keeps its own failures to itself.
    ///
    /// The poll runs every few hours, so its failures are a tunnel, a captive
    /// portal or an update server having an afternoon, and none of them is
    /// something the reader can act on. Answering with `Failed` anyway puts that
    /// in the update strip, which is the one place the app speaks about its own
    /// trustworthiness: a strip that cries failure at a reader who did nothing
    /// teaches them to ignore the strip, and then it cannot tell them that a
    /// signed build is staged and waiting. Only a check somebody asked for may
    /// take the strip over. The error still counts, because the backoff behind
    /// this loop is what stops a broken update server being asked every minute.
    pub async fn poll(&self) -> Result<()> {
        let shown = self.status();
        let result = self.check().await;
        if result.is_err() && self.status().phase == UpdatePhase::Failed {
            self.set_status(shown);
        }
        result
    }

    pub fn spawn_auto_poll(&self) -> JoinHandle<()> {
        let runtime = self.clone();
        tokio::spawn(async move {
            if !updater_configured() {
                return;
            }
            let mut consecutive_failures = 0u32;
            loop {
                if runtime.poll().await.is_err() {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                } else {
                    consecutive_failures = 0;
                }
                tokio::time::sleep(retry_delay(consecutive_failures)).await;
            }
        })
    }

    fn status(&self) -> UpdaterStatus {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_status(&self, status: UpdaterStatus) {
        *self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status.clone();
        (self.inner.status)(status);
    }

    fn set_unsupported(&self, reason: String) {
        self.set_status(UpdaterStatus::new(UpdatePhase::Unsupported).with_error(reason));
    }

    fn fail(&self, error: anyhow::Error) -> Result<()> {
        let message = format!("{error:#}");
        self.set_status(UpdaterStatus::new(UpdatePhase::Failed).with_error(message));
        Err(error)
    }

    fn validate_candidate(&self, manifest: &UpdateManifest) -> Result<()> {
        validate_manifest(manifest, CHANNEL)?;
        ensure!(
            manifest.version.pre.is_empty() && manifest.version.build.is_empty(),
            "Stable manifest has a prerelease or build version."
        );
        Ok(())
    }

    fn persist_success(&self, etag: Option<String>, version: String) {
        let _ = mutate_update_state(|state| {
            state.etag = etag;
            state.latest_version = Some(version);
        });
    }

    async fn fetch_manifest(&self) -> Result<FetchedManifest> {
        let manifest_url = http_url(MANIFEST_URL).context("Invalid update manifest URL.")?;
        let signature_url = signature_url(&manifest_url)?;
        let cache_dir = update_cache_dir()?;
        let cached_manifest = cache_dir.join("manifest.json");
        let state = read_update_state();
        let etag = state.etag.as_deref().and_then(normalize_etag);
        let mut request = self
            .inner
            .client
            .get(manifest_url)
            .header(USER_AGENT, concat!("k8s-gpui/", env!("CARGO_PKG_VERSION")));
        if let Some(etag) = etag.as_deref() {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .context("Update manifest request failed.")?;
        let not_modified = response.status() == reqwest::StatusCode::NOT_MODIFIED;
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_etag);
        let manifest_bytes = if not_modified {
            read_cached_file(&cached_manifest, MAX_MANIFEST_SIZE).with_context(|| {
                format!(
                    "Cached manifest is unavailable at {}",
                    cached_manifest.display()
                )
            })?
        } else {
            let response = ensure_success(response, "Update manifest").await?;
            read_limited(response, MAX_MANIFEST_SIZE)
                .await
                .context("Update manifest response is too large.")?
        };
        let signature = send_successful_request(
            &self.inner.client,
            signature_url,
            "Update manifest signature",
        )
        .await?;
        let signature = read_limited(signature, MAX_SIGNATURE_SIZE)
            .await
            .context("Update manifest signature response is too large.")?;
        let manifest = parse_verified_manifest(&manifest_bytes, &signature)?;
        if !not_modified {
            k8s_core::atomic_file::create_private_dir_all(&cache_dir).with_context(|| {
                format!(
                    "Failed to create the update cache at {}",
                    cache_dir.display()
                )
            })?;
            k8s_core::atomic_file::write_atomic(&cached_manifest, &manifest_bytes)
                .with_context(|| format!("Failed to cache {}", cached_manifest.display()))?;
        }
        Ok(FetchedManifest {
            manifest,
            etag: if not_modified { etag } else { response_etag },
        })
    }

    async fn download_and_stage(
        &self,
        version: &str,
        version_dir: &str,
        artifacts: &BinaryArtifacts,
        managed: InstallPlan,
    ) -> Result<PathBuf> {
        let plan = InstallPlan::new(managed.root.clone(), version_dir.to_owned())?;
        if let Some(executable) =
            recover_existing_installation_with_cleanup(version, artifacts, &plan).await?
        {
            return Ok(executable);
        }
        let total = artifacts.app.size;
        let app = self
            .ensure_cached_binary(version, version_dir, &artifacts.app, 0, total)
            .await?;
        stage_and_activate(&plan, version, version_dir, artifacts, &app).await
    }

    async fn ensure_cached_binary(
        &self,
        version: &str,
        version_dir: &str,
        artifact: &UpdateArtifact,
        progress_offset: u64,
        total: u64,
    ) -> Result<PathBuf> {
        ensure!(artifact.size <= MAX_ARTIFACT_SIZE, "Artifact is too large.");
        let artifact_dir = update_cache_dir()?.join(version_dir);
        k8s_core::atomic_file::create_private_dir_all(&artifact_dir).with_context(|| {
            format!(
                "Failed to create the update cache at {}",
                artifact_dir.display()
            )
        })?;
        let destination = artifact_dir.join(&artifact.name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                let cached = destination.clone();
                let expected_size = artifact.size;
                let expected_hash = decode_sha256(&artifact.sha256)?;
                let valid = tokio::task::spawn_blocking(move || {
                    verify_binary_file(&cached, expected_size, &expected_hash)
                })
                .await
                .context("Cached binary validation task failed.")?
                .is_ok();
                if valid {
                    self.report_download(version, artifact.size, progress_offset, total);
                    return Ok(destination);
                }
                remove_cached_entry(&destination)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to inspect the cached binary."),
        }

        let url = http_url(&artifact.url).context("Invalid artifact URL.")?;
        let response = send_successful_request(
            &self.inner.client,
            url,
            &format!("{} artifact", artifact.name),
        )
        .await?;
        let mut response = response;
        if let Some(content_length) = response.content_length() {
            ensure!(
                content_length <= MAX_ARTIFACT_SIZE,
                "Artifact is too large."
            );
            ensure!(
                content_length == artifact.size,
                "Artifact content length does not match the manifest."
            );
        }
        let part_path = artifact_dir.join(format!("{}.part", artifact.name));
        remove_cached_entry(&part_path)?;
        let mut part = PartGuard::new(part_path.clone());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options
            .open(&part_path)
            .await
            .with_context(|| format!("Failed to create {}", part_path.display()))?;
        let expected_hash = decode_sha256(&artifact.sha256)?;
        let mut hasher = Sha256::new();
        let mut downloaded = 0u64;
        let mut last_report = Instant::now();
        self.report_download(version, downloaded, progress_offset, total);
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Artifact response stream failed.")?
        {
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .context("Artifact size overflow.")?;
            ensure!(
                downloaded <= artifact.size,
                "Artifact exceeds the manifest size."
            );
            ensure!(downloaded <= MAX_ARTIFACT_SIZE, "Artifact is too large.");
            file.write_all(&chunk)
                .await
                .context("Failed to write the artifact.")?;
            hasher.update(&chunk);
            if last_report.elapsed() >= Duration::from_millis(250) || downloaded == artifact.size {
                self.report_download(version, downloaded, progress_offset, total);
                last_report = Instant::now();
            }
        }
        file.flush()
            .await
            .context("Failed to flush the artifact.")?;
        file.sync_all()
            .await
            .context("Failed to sync the artifact.")?;
        drop(file);
        ensure!(
            downloaded == artifact.size,
            "Artifact size does not match the manifest."
        );
        let actual_hash = hasher.finalize();
        ensure!(
            actual_hash.as_slice() == expected_hash.as_slice(),
            "Artifact SHA-256 does not match the manifest."
        );
        ensure_elf_binary(&part_path)?;
        tokio::fs::rename(&part_path, &destination)
            .await
            .with_context(|| format!("Failed to finalize {}", destination.display()))?;
        part.disarm();
        Ok(destination)
    }

    fn report_download(&self, version: &str, downloaded: u64, progress_offset: u64, total: u64) {
        let downloaded = progress_offset.saturating_add(downloaded);
        self.set_status(
            UpdaterStatus::new(UpdatePhase::Downloading)
                .with_version(version)
                .with_progress(Some(downloaded as f32 / total as f32)),
        );
    }
}

fn select_binary_artifacts(manifest: &UpdateManifest) -> Result<Option<BinaryArtifacts>> {
    let mut app = None;
    let mut target_count = 0usize;
    for artifact in &manifest.artifacts {
        if artifact.os != UPDATE_TARGET_OS || artifact.arch != UPDATE_TARGET_ARCH {
            continue;
        }
        target_count += 1;
        ensure!(artifact.size <= MAX_ARTIFACT_SIZE, "Artifact is too large.");
        let _ = http_url(&artifact.url).context("Invalid artifact URL.")?;
        let expected_name = app_asset_name();
        match artifact.name.as_str() {
            name if name == expected_name => {
                ensure!(
                    app.replace(artifact.clone()).is_none(),
                    "Duplicate {expected_name} artifact."
                );
            }
            name => bail!("Unexpected linux/{UPDATE_TARGET_ARCH} artifact: {name}"),
        }
    }
    if target_count == 0 {
        return Ok(None);
    }
    ensure!(
        target_count == 1,
        "The linux/{UPDATE_TARGET_ARCH} manifest must contain one artifact."
    );
    let missing = format!("Manifest is missing the {} artifact.", app_asset_name());
    Ok(Some(BinaryArtifacts {
        app: app.context(missing)?,
    }))
}

fn managed_installation(expected_version_dir: &str) -> Result<InstallPlan, String> {
    let environment = InstallEnvironment::current();
    let data_dir = k8s_core::paths::data_dir();
    let current_exe = std::env::current_exe().ok();
    managed_installation_in(
        data_dir.as_deref(),
        current_exe.as_deref(),
        expected_version_dir,
        environment,
    )
}

fn managed_installation_in(
    data_dir: Option<&Path>,
    current_exe: Option<&Path>,
    expected_version_dir: &str,
    environment: InstallEnvironment,
) -> Result<InstallPlan, String> {
    if !environment.supported_platform {
        return Err("Automatic updates are not available on this platform. Use Linux or update through the package or source that installed the app.".to_owned());
    }
    if environment.debug_build {
        return Err("Automatic updates are not available in debug builds. Run a release build to enable update checks and installation.".to_owned());
    }
    if environment.flatpak {
        return Err("The running Flatpak package cannot update itself. Update through Flatpak, then restart the app.".to_owned());
    }
    if environment.app_image {
        return Err("The running AppImage cannot update itself. Download a newer AppImage, then restart the app.".to_owned());
    }
    let data_dir = data_dir.ok_or_else(|| {
        "The application data directory was unavailable. Set XDG_DATA_HOME to a writable directory, then try again."
            .to_owned()
    })?;
    let current_exe = current_exe.ok_or_else(|| {
        "The current executable path was unavailable. Reinstall K8s Studio, then try again."
            .to_owned()
    })?;
    let root = data_dir.join("installations");
    let root = fs::canonicalize(&root).map_err(|error| {
        format!(
            "The managed installation directory was unavailable. Check the app data directory, then try again. Detail: {error}"
        )
    })?;
    let current_exe_text = current_exe.to_string_lossy();
    if current_exe_text.contains("/.flatpak/") {
        return Err("The running Flatpak package cannot update itself. Update through Flatpak, then restart the app.".to_owned());
    }
    if current_exe_text.contains("/.mount_") {
        return Err("The running AppImage cannot update itself. Download a newer AppImage, then restart the app.".to_owned());
    }
    let current_exe = fs::canonicalize(current_exe).map_err(|error| {
        format!(
            "The current executable was not readable. Reinstall K8s Studio, then try again. Detail: {error}"
        )
    })?;
    if current_exe.file_name().and_then(|name| name.to_str()) != Some("k8s-app") {
        return Err(format!(
            "The running app is not a managed k8s-app installation. {MANAGED_INSTALL_ACTION}"
        ));
    }
    let version_dir = current_exe.parent().ok_or_else(|| {
        "The current executable has no parent directory. Reinstall K8s Studio, then try again."
            .to_owned()
    })?;
    if version_dir.parent() != Some(root.as_path()) {
        return Err(format!(
            "The running app is outside the managed installation directory. {MANAGED_INSTALL_ACTION}"
        ));
    }
    if version_dir.file_name().and_then(|name| name.to_str()) != Some(expected_version_dir) {
        return Err(format!(
            "The managed app version did not match the running binary. {MANAGED_INSTALL_ACTION}"
        ));
    }
    Builder::new()
        .prefix(".write-check-")
        .tempdir_in(&root)
        .map_err(|error| {
            format!(
                "The managed installation directory was not writable. Check its permissions, then try again. Detail: {error}"
            )
        })?;
    InstallPlan::new(root, expected_version_dir.to_owned()).map_err(|error| error.to_string())
}

fn verify_signature(bytes: &[u8], signature: &[u8], key: &VerifyingKey) -> Result<()> {
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| anyhow!("Detached signature must be exactly 64 bytes."))?;
    let signature = Signature::from_bytes(&signature);
    key.verify_strict(bytes, &signature)
        .context("Update manifest signature verification failed.")?;
    Ok(())
}

fn parse_verified_manifest(bytes: &[u8], signature: &[u8]) -> Result<UpdateManifest> {
    parse_verified_manifest_with_key(bytes, signature, &public_key()?)
}

fn parse_verified_manifest_with_key(
    bytes: &[u8],
    signature: &[u8],
    public_key: &VerifyingKey,
) -> Result<UpdateManifest> {
    verify_signature(bytes, signature, public_key)?;
    parse_and_validate_manifest(bytes, CHANNEL).map_err(anyhow::Error::new)
}

fn public_key() -> Result<VerifyingKey> {
    let encoded =
        PUBLIC_KEY_HEX.context("K8S_GPUI_UPDATE_PUBLIC_KEY was not embedded at compile time.")?;
    let bytes = hex::decode(encoded).context("Update public key is not valid hex.")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("Update public key must contain exactly 32 bytes."))?;
    VerifyingKey::from_bytes(&bytes).context("Update public key is invalid.")
}

fn signature_url(manifest_url: &Url) -> Result<Url> {
    let mut signature = manifest_url.clone();
    let path = format!("{}.sig", manifest_url.path());
    signature.set_path(&path);
    Ok(signature)
}

fn http_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context(
        "The update URL was invalid. Configure a valid HTTPS update URL, then try again.",
    )?;
    ensure!(
        url.scheme() == "https",
        "The update URL did not use HTTPS. Configure an HTTPS update URL, then try again."
    );
    Ok(url)
}

fn normalize_etag(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_ETAG_SIZE {
        return None;
    }
    reqwest::header::HeaderValue::from_str(value)
        .ok()
        .map(|_| value.to_owned())
}

async fn send_successful_request(
    client: &reqwest::Client,
    url: Url,
    description: &str,
) -> Result<reqwest::Response> {
    let response = client
        .get(url)
        .header(USER_AGENT, concat!("k8s-gpui/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .with_context(|| format!("{description} request failed."))?;
    ensure_success(response, description).await
}

async fn ensure_success(
    response: reqwest::Response,
    description: &str,
) -> Result<reqwest::Response> {
    ensure!(
        response.status().is_success(),
        "{description} returned HTTP {}.",
        response.status()
    );
    Ok(response)
}

async fn read_limited(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if let Some(content_length) = response.content_length()
        && content_length > max as u64
    {
        bail!("Response body exceeds {max} bytes.");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("Response stream failed.")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= max,
            "Response body exceeds {max} bytes."
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn read_cached_file(path: &Path, max: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file(),
        "Cached manifest is not a regular file."
    );
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    ensure!(length <= max as u64, "Cached file exceeds {max} bytes.");
    let mut bytes = Vec::with_capacity(length as usize);
    file.by_ref()
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "Cached file exceeds {max} bytes.");
    Ok(bytes)
}

fn mutate_update_state(update: impl FnOnce(&mut k8s_core::update::UpdateStateDto)) -> Result<()> {
    let data_dir =
        k8s_core::paths::data_dir().context("The application data directory is unavailable.")?;
    let _lock = acquire_update_lock(&data_dir.join("installations"))?;
    let mut state = read_update_state();
    update(&mut state);
    write_update_state(&state)?;
    Ok(())
}

fn update_cache_dir() -> Result<PathBuf> {
    k8s_core::paths::cache_dir()
        .map(|path| path.join("updates"))
        .context("Application cache directory is unavailable.")
}

fn decode_sha256(value: &str) -> Result<[u8; 32]> {
    ensure!(
        k8s_core::update::is_valid_sha256(value),
        "Manifest SHA-256 is invalid."
    );
    hex::decode(value)
        .context("Manifest SHA-256 is not valid hex.")?
        .try_into()
        .map_err(|_| anyhow!("Manifest SHA-256 must contain 32 bytes."))
}

fn verify_binary_file(path: &Path, expected_size: u64, expected_hash: &[u8; 32]) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file(),
        "Binary is not a regular file."
    );
    ensure!(
        metadata.len() == expected_size,
        "Binary size does not match."
    );
    let actual_hash = hash_file(path)?;
    ensure!(
        actual_hash.as_slice() == expected_hash.as_slice(),
        "Binary SHA-256 does not match."
    );
    ensure_elf_binary(path)
}

fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

fn ensure_elf_binary(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    ensure!(&magic == b"\x7fELF", "Artifact is not a raw ELF binary.");
    Ok(())
}

fn remove_cached_entry(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            bail!("Cache entry is a directory: {}", path.display())
        }
        Ok(_) => fs::remove_file(path)
            .with_context(|| format!("Failed to remove cache entry {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to inspect cache entry {}", path.display()))
        }
    }
}

fn copy_and_validate_binary(
    source: &Path,
    destination: &Path,
    size: u64,
    hash: &[u8; 32],
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.file_type().is_file(),
        "Cached binary is not a regular file."
    );
    ensure!(metadata.len() == size, "Cached binary size does not match.");
    fs::copy(source, destination)
        .with_context(|| format!("Failed to stage {}", destination.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    verify_binary_file(destination, size, hash)?;
    File::open(destination)?.sync_all()?;
    Ok(())
}

async fn recover_existing_installation(
    version: &str,
    artifacts: &BinaryArtifacts,
    plan: &InstallPlan,
) -> Result<Option<PathBuf>> {
    let exists = path_exists(&plan.version_dir)?;
    let protected = installation_is_current(plan, &plan.version_dir)
        || running_installation_name(&plan.root).as_deref() == Some(plan.version_dir_name.as_str());
    let complete = if exists {
        installation_is_complete(&plan.version_dir, artifacts)?
    } else {
        false
    };
    match existing_version_action(exists, protected, complete) {
        ExistingVersionAction::Stage => Ok(None),
        ExistingVersionAction::Reuse => {
            let app = plan.version_dir.join("k8s-app");
            if let Err(error) = smoke_test(&app, version).await {
                if protected {
                    return Err(error);
                }
                remove_installation_entry(&plan.version_dir).with_context(|| {
                    format!(
                        "Failed to remove invalid installation {}.",
                        plan.version_dir.display()
                    )
                })?;
                return Ok(None);
            }
            switch_current(plan)?;
            Ok(Some(app))
        }
        ExistingVersionAction::Remove => {
            remove_installation_entry(&plan.version_dir).with_context(|| {
                format!(
                    "Failed to remove incomplete installation {}.",
                    plan.version_dir.display()
                )
            })?;
            Ok(None)
        }
        ExistingVersionAction::PreserveCurrent => {
            bail!(
                "The current managed installation is incomplete and was not removed. Run `k8s-app install-user` again."
            )
        }
    }
}

async fn recover_existing_installation_with_cleanup(
    version: &str,
    artifacts: &BinaryArtifacts,
    plan: &InstallPlan,
) -> Result<Option<PathBuf>> {
    let _lock = acquire_update_lock(&plan.root)?;
    let executable = recover_existing_installation(version, artifacts, plan).await?;
    if executable.is_some() {
        cleanup_after_switch(plan, None);
    }
    Ok(executable)
}

async fn activate_staged_update(
    plan: &InstallPlan,
    version: &str,
    version_dir: &str,
    artifacts: &BinaryArtifacts,
    staging: &Path,
    staging_guard: &mut DirectoryGuard,
) -> Result<PathBuf> {
    if let Some(executable) = recover_existing_installation(version, artifacts, plan).await? {
        cleanup_after_switch(plan, Some(staging));
        return Ok(executable);
    }
    if let Err(first) = activate_staged_version(staging, plan) {
        if let Some(executable) = recover_existing_installation(version, artifacts, plan).await? {
            cleanup_after_switch(plan, Some(staging));
            return Ok(executable);
        }
        activate_staged_version(staging, plan).map_err(|second| {
            anyhow!(
                "Failed to activate version directory {version_dir}: {second}. The first attempt failed: {first}."
            )
        })?;
    }
    staging_guard.disarm();
    let mut installed_guard = DirectoryGuard::managed(plan);
    switch_current(plan)?;
    installed_guard.disarm();
    cleanup_after_switch(plan, Some(staging));
    Ok(plan.version_dir.join("k8s-app"))
}

async fn stage_and_activate(
    plan: &InstallPlan,
    version: &str,
    version_dir: &str,
    artifacts: &BinaryArtifacts,
    app_cache: &Path,
) -> Result<PathBuf> {
    if let Some(executable) =
        recover_existing_installation_with_cleanup(version, artifacts, plan).await?
    {
        return Ok(executable);
    }
    let staging = Builder::new()
        .prefix(".staging-")
        .tempdir_in(&plan.root)
        .with_context(|| {
            format!(
                "Failed to create the staging directory in {}",
                plan.root.display()
            )
        })?;
    let app_destination = staging.path().join("k8s-app");
    let app_source = app_cache.to_owned();
    let app_target = app_destination.clone();
    let app_size = artifacts.app.size;
    let app_hash = decode_sha256(&artifacts.app.sha256)?;
    let app_task = tokio::task::spawn_blocking(move || {
        copy_and_validate_binary(&app_source, &app_target, app_size, &app_hash)
    });
    let app_result = app_task.await;
    app_result.context("k8s-app staging task failed.")??;
    smoke_test(&app_destination, version).await?;
    File::open(staging.path())?.sync_all()?;
    let staging_path = staging.keep();
    let mut staging_guard = DirectoryGuard::new(staging_path.clone());
    let executable = {
        let _lock = acquire_update_lock(&plan.root)?;
        activate_staged_update(
            plan,
            version,
            version_dir,
            artifacts,
            &staging_path,
            &mut staging_guard,
        )
        .await?
    };
    drop(staging_guard);
    Ok(executable)
}

async fn smoke_test(path: &Path, version: &str) -> Result<()> {
    let output = timeout(
        SMOKE_TEST_TIMEOUT,
        tokio::process::Command::new(path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| format!("{} --version timed out.", path.display()))?
    .with_context(|| format!("Failed to execute {}", path.display()))?;
    ensure!(
        output.status.success(),
        "{} --version failed.",
        path.display()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    ensure!(
        stdout.contains(version),
        "{} --version did not report {version}.",
        path.display()
    );
    Ok(())
}

fn sync_directory(path: &Path) {
    let _ = File::open(path).and_then(|directory| directory.sync_all());
}

fn activate_staged_version(staging: &Path, plan: &InstallPlan) -> Result<()> {
    fs::rename(staging, &plan.version_dir).with_context(|| {
        format!(
            "Failed to activate the managed version directory: {}",
            plan.version_dir.display()
        )
    })?;
    sync_directory(&plan.root);
    Ok(())
}

#[cfg(unix)]
fn switch_current(plan: &InstallPlan) -> Result<()> {
    use std::os::unix::fs::symlink;

    let holder = Builder::new()
        .prefix(".current-")
        .tempdir_in(&plan.root)
        .with_context(|| {
            format!(
                "Failed to stage the current link in {}",
                plan.root.display()
            )
        })?;
    let staged_link = holder.path().join("current");
    symlink(&plan.version_dir_name, &staged_link)
        .context("Failed to create the staged current symlink.")?;
    fs::rename(&staged_link, &plan.current)
        .context("Failed to atomically replace the current symlink.")?;
    sync_directory(&plan.root);
    Ok(())
}

#[cfg(not(unix))]
fn switch_current(_plan: &InstallPlan) -> Result<()> {
    bail!("Atomic current symlink switching is supported only on Unix.")
}

fn retry_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return POLL_INTERVAL;
    }
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    RETRY_BASE
        .saturating_mul(1u32 << exponent)
        .min(POLL_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn update_lock_rejects_a_contender_until_the_owner_process_exits() {
        const ROOT_ENV: &str = "K8S_GPUI_TEST_UPDATE_LOCK_ROOT";
        const READY_ENV: &str = "K8S_GPUI_TEST_UPDATE_LOCK_READY";
        const HOLD_ENV: &str = "K8S_GPUI_TEST_UPDATE_LOCK_HOLD";

        if std::env::var_os(HOLD_ENV).is_some() {
            let root = std::env::var_os(ROOT_ENV).expect("root path");
            let _lock = acquire_update_lock(Path::new(&root)).expect("child lock");
            fs::write(std::env::var_os(READY_ENV).expect("ready path"), b"ready")
                .expect("ready file");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().join("installations");
        let ready = directory.path().join("ready");
        fs::create_dir_all(&root).expect("root");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "updater::tests::update_lock_rejects_a_contender_until_the_owner_process_exits",
            ])
            .env(ROOT_ENV, &root)
            .env(READY_ENV, &ready)
            .env(HOLD_ENV, "1")
            .spawn()
            .expect("child process");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("child status") {
                panic!("child exited before locking: {status}");
            }
            if started.elapsed() >= Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not acquire the update lock");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let contention = try_acquire_update_lock(&root);
        child.kill().expect("kill child");
        child.wait().expect("wait for child");
        assert!(contention.is_err(), "competing lock must be rejected");
        let _released = try_acquire_update_lock(&root).expect("released lock");
    }

    #[test]
    fn orphan_version_recovery_is_fault_injection_safe() {
        assert_eq!(
            existing_version_action(false, false, false),
            ExistingVersionAction::Stage
        );
        assert_eq!(
            existing_version_action(false, true, false),
            ExistingVersionAction::Stage
        );
        assert_eq!(
            existing_version_action(true, false, true),
            ExistingVersionAction::Reuse
        );
        assert_eq!(
            existing_version_action(true, false, false),
            ExistingVersionAction::Remove
        );
        assert_eq!(
            existing_version_action(true, true, true),
            ExistingVersionAction::Reuse
        );
        assert_eq!(
            existing_version_action(true, true, false),
            ExistingVersionAction::PreserveCurrent
        );
    }

    fn manifest_bytes() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema": 1,
            "channel": "stable",
            "version": "1.2.3",
            "published_at": "2026-09-24T00:00:00Z",
            "artifacts": [
                {
                    "os": "linux",
                    "arch": std::env::consts::ARCH,
                    "name": app_asset_name(),
                    "url": "https://example.invalid/k8s-app",
                    "size": 4,
                    "sha256": "0".repeat(64)
                }
            ]
        }))
        .expect("manifest JSON")
    }

    #[test]
    fn manifest_is_parsed_only_after_signature_verification() {
        let manifest = manifest_bytes();
        let key = signing_key();
        let signature = key.sign(&manifest).to_bytes();
        let parsed = parse_verified_manifest_with_key(&manifest, &signature, &key.verifying_key())
            .expect("verified manifest");
        assert_eq!(parsed.version, parse_version("1.2.3").expect("version"));
        assert_eq!(
            select_binary_artifacts(&parsed)
                .expect("selection")
                .expect("target")
                .app
                .name,
            app_asset_name()
        );
    }

    #[test]
    fn signature_failure_rejects_manifest() {
        let manifest = manifest_bytes();
        let key = signing_key();
        let mut signature = key.sign(&manifest).to_bytes();
        signature[0] ^= 1;
        assert!(
            parse_verified_manifest_with_key(&manifest, &signature, &key.verifying_key()).is_err()
        );
    }

    #[test]
    fn hash_mismatch_is_rejected() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("k8s-app");
        fs::write(&path, b"\x7fELFpayload").expect("write");
        assert!(verify_binary_file(&path, 11, &[0; 32]).is_err());
        assert!(verify_binary_file(&path, 12, &[0; 32]).is_err());
    }

    #[test]
    fn managed_path_policy_rejects_debug_flatpak_appimage_and_foreign_paths() {
        let directory = tempfile::tempdir().expect("temp dir");
        let data = directory.path().join("data");
        let root = data.join("installations");
        let version = root.join("v1.2.3");
        fs::create_dir_all(&version).expect("version dir");
        let executable = version.join("k8s-app");
        fs::write(&executable, b"binary").expect("executable");
        let release = InstallEnvironment {
            supported_platform: true,
            debug_build: false,
            flatpak: false,
            app_image: false,
        };
        assert_eq!(
            managed_installation_in(Some(&data), Some(&executable), "v1.2.3", release)
                .expect("managed path")
                .version_dir_name,
            "v1.2.3"
        );
        let unsupported = InstallEnvironment {
            supported_platform: false,
            ..release
        };
        let error = managed_installation_in(Some(&data), Some(&executable), "v1.2.3", unsupported)
            .expect_err("unsupported platforms must reject managed updates");
        assert!(error.contains("platform"));
        assert!(error.contains("Use Linux"));

        let debug = InstallEnvironment {
            debug_build: true,
            ..release
        };
        let error = managed_installation_in(Some(&data), Some(&executable), "v1.2.3", debug)
            .expect_err("debug builds must reject managed updates");
        assert!(error.contains("release build"));
        let flatpak = InstallEnvironment {
            flatpak: true,
            ..release
        };
        let error = managed_installation_in(Some(&data), Some(&executable), "v1.2.3", flatpak)
            .expect_err("Flatpak must reject managed updates");
        assert!(error.contains("Update through Flatpak"));
        let app_image = InstallEnvironment {
            app_image: true,
            ..release
        };
        let error = managed_installation_in(Some(&data), Some(&executable), "v1.2.3", app_image)
            .expect_err("AppImage must reject managed updates");
        assert!(error.contains("Download a newer AppImage"));
        let error = managed_installation_in(Some(&data), Some(&executable), "v9.9.9", release)
            .expect_err("version mismatches must reject managed updates");
        assert!(error.contains("k8s-app install-user"));
        let foreign = data.join("other").join("v1.2.3");
        fs::create_dir_all(&foreign).expect("foreign dir");
        let foreign_executable = foreign.join("k8s-app");
        fs::write(&foreign_executable, b"binary").expect("foreign executable");
        let error =
            managed_installation_in(Some(&data), Some(&foreign_executable), "v1.2.3", release)
                .expect_err("foreign paths must reject managed updates");
        assert!(error.contains("outside the managed installation directory"));
        assert!(error.contains("k8s-app install-user"));
    }

    #[test]
    fn update_urls_require_https() {
        assert!(http_url("https://example.invalid/manifest.json").is_ok());
        let error = http_url("http://example.invalid/manifest.json")
            .expect_err("plain HTTP update URLs must be rejected");
        let message = format!("{error:#}");
        assert!(message.contains("HTTPS"));
        assert!(message.contains("then try again"));
    }
}
