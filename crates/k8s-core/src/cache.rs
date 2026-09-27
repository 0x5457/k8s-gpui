//! Disk cache for object snapshots and discovery catalogs.
//!
//! Secrets never reach disk. Cache entries are filtered on write and load.
//! Invalid entries are misses. Stale entries can still be shown.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kube::api::ApiResource;
use kube::core::DynamicObject;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::atomic_file::{create_private_dir_all, write_atomic as write_atomic_bytes};
use crate::cluster::ClusterId;
use crate::discovery::ResourceCatalog;

/// TTL for snapshot and discovery caches. Older entries are stale.
pub const CACHE_TTL: Duration = Duration::from_hours(24);

/// Disk format version. A structural change increments it. Old files are misses.
const FORMAT_VERSION: u32 = 1;

/// Discovery cache file name.
const DISCOVERY_FILE: &str = "discovery.json";

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Cache read or write failed: {0}. Check the cache directory and try again.")]
    Io(#[from] std::io::Error),

    #[error("Cache serialization failed: {0}. Check the cached data and try again.")]
    Serialize(#[from] serde_json::Error),
}

/// GVR cache key: group, version, and plural. The core group is an empty string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GvrKey {
    pub group: String,
    pub version: String,
    pub plural: String,
}

impl GvrKey {
    pub fn new(
        group: impl Into<String>,
        version: impl Into<String>,
        plural: impl Into<String>,
    ) -> Self {
        Self {
            group: group.into(),
            version: version.into(),
            plural: plural.into(),
        }
    }

    pub fn from_api_resource(resource: &ApiResource) -> Self {
        Self::new(
            resource.group.clone(),
            resource.version.clone(),
            resource.plural.clone(),
        )
    }

    /// File name without `.json`: `core.v1.pods` or `apps.v1.deployments`.
    ///
    /// The core group maps to `core`. Other characters become `_` to prevent path traversal.
    pub fn file_stem(&self) -> String {
        let group = if self.group.is_empty() {
            "core"
        } else {
            &self.group
        };
        format!(
            "{}.{}.{}",
            sanitize(group),
            sanitize(&self.version),
            sanitize(&self.plural)
        )
    }
}

impl From<&ApiResource> for GvrKey {
    fn from(resource: &ApiResource) -> Self {
        Self::from_api_resource(resource)
    }
}

fn sanitize(part: &str) -> String {
    let mut out: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Return true for the core `secrets` GVR, ignoring case.
/// The file one GVR's snapshot lives in.
///
/// `path`, `save` and `load` each need the name, and `save` and `load` also hand it
/// to the store to remove a Secret GVR's file - so a fourth spelling of this
/// would be a fourth way for a stale snapshot to survive.
fn file_name(gvr: &GvrKey) -> String {
    format!("{}.json", gvr.file_stem())
}

pub fn is_secret_gvr(gvr: &GvrKey) -> bool {
    (gvr.group.is_empty() || gvr.group.eq_ignore_ascii_case("core"))
        && gvr.plural.eq_ignore_ascii_case("secrets")
}

/// Return true for a Secret object, ignoring kind case and group.
pub fn is_secret_object(obj: &DynamicObject) -> bool {
    obj.types
        .as_ref()
        .is_some_and(|types| types.kind.eq_ignore_ascii_case("secret"))
}

/// Snapshot loaded from disk.
#[derive(Clone, Debug)]
pub struct CachedSnapshot {
    /// Objects with Secrets removed.
    pub objects: Vec<Arc<DynamicObject>>,
    /// Save time from the file field `saved_at`.
    pub saved_at: SystemTime,
    /// True when `saved_at` is older than [`CACHE_TTL`]. The data is still returned.
    pub stale: bool,
}

#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    cluster_uid: String,
    saved_at: u64,
    gvr: GvrKey,
    objects: Vec<DynamicObject>,
}

#[derive(Serialize)]
struct SnapshotFileRef<'a> {
    version: u32,
    cluster_uid: &'a str,
    saved_at: u64,
    gvr: &'a GvrKey,
    objects: Vec<&'a DynamicObject>,
}

/// Discovery catalog loaded from disk.
#[derive(Clone, Debug)]
pub struct CachedCatalog {
    pub catalog: ResourceCatalog,
    pub saved_at: SystemTime,
    pub stale: bool,
}

#[derive(Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    cluster_uid: String,
    saved_at: u64,
    server_version: String,
    catalog: ResourceCatalog,
}

#[derive(Clone, Debug)]
struct Store {
    root: PathBuf,
    cluster_id: ClusterId,
    cluster_uid: String,
    enabled: bool,
}

impl Store {
    /// Cache is active only when the setting is enabled and the cluster UID is known.
    fn active(&self) -> bool {
        self.enabled && !self.cluster_uid.is_empty()
    }

    fn dir(&self) -> PathBuf {
        self.root.join(self.cluster_id.to_string())
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir().join(file)
    }

    fn write<T: Serialize>(&self, file: &str, value: &T) -> Result<(), CacheError> {
        if !self.active() {
            return Ok(());
        }
        create_private_dir_all(&self.root)?;
        let dir = self.dir();
        create_private_dir_all(&dir)?;
        let json = serde_json::to_vec(value)?;
        write_atomic_bytes(&dir.join(file), &json)?;
        Ok(())
    }

    fn read<T: DeserializeOwned>(&self, file: &str) -> Option<T> {
        if !self.active() {
            return None;
        }
        let path = self.path(file);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "Failed to read cache. Treating it as a cache miss.");
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "Cache is corrupt. Removing it and treating it as a cache miss.");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn remove(&self, file: &str) {
        let _ = fs::remove_file(self.path(file));
    }
}

/// Snapshot cache path: `<root>/<cluster-id>/<gvr>.json`.
#[derive(Clone, Debug)]
pub struct SnapshotCache {
    store: Store,
}

impl SnapshotCache {
    pub fn new(
        root: impl Into<PathBuf>,
        cluster_id: ClusterId,
        cluster_uid: impl Into<String>,
        enabled: bool,
    ) -> Self {
        Self {
            store: Store {
                root: root.into(),
                cluster_id,
                cluster_uid: cluster_uid.into(),
                enabled,
            },
        }
    }

    /// Snapshot cache file path.
    pub fn path(&self, gvr: &GvrKey) -> PathBuf {
        self.store.path(&file_name(gvr))
    }

    /// Save atomically. Secret GVRs are skipped and old files are removed.
    pub fn save(&self, gvr: &GvrKey, objects: &[Arc<DynamicObject>]) -> Result<(), CacheError> {
        let file_name = file_name(gvr);
        if is_secret_gvr(gvr) {
            self.store.remove(&file_name);
            return Ok(());
        }
        if !self.store.active() {
            return Ok(());
        }
        let file = SnapshotFileRef {
            version: FORMAT_VERSION,
            cluster_uid: &self.store.cluster_uid,
            saved_at: unix_secs(SystemTime::now()),
            gvr,
            objects: objects
                .iter()
                .map(|obj| obj.as_ref())
                .filter(|obj| !is_secret_object(obj))
                .collect(),
        };
        self.store.write(&file_name, &file)
    }

    /// Load a snapshot. Invalid or Secret data is removed and returns `None`.
    pub fn load(&self, gvr: &GvrKey) -> Option<CachedSnapshot> {
        let file_name = file_name(gvr);
        if is_secret_gvr(gvr) {
            self.store.remove(&file_name);
            return None;
        }
        let file: SnapshotFile = self.store.read(&file_name)?;
        if file.version != FORMAT_VERSION
            || file.gvr != *gvr
            || file.cluster_uid != self.store.cluster_uid
        {
            self.store.remove(&file_name);
            return None;
        }
        let Some(saved_at) = from_unix_secs(file.saved_at) else {
            self.store.remove(&file_name);
            return None;
        };
        let objects = file
            .objects
            .into_iter()
            .filter(|obj| !is_secret_object(obj))
            .map(Arc::new)
            .collect();
        Some(CachedSnapshot {
            objects,
            saved_at,
            stale: is_stale(saved_at, SystemTime::now()),
        })
    }
}

/// Discovery cache path: `<root>/<cluster-id>/discovery.json`.
#[derive(Clone, Debug)]
pub struct DiscoveryCache {
    store: Store,
}

impl DiscoveryCache {
    pub fn new(
        root: impl Into<PathBuf>,
        cluster_id: ClusterId,
        cluster_uid: impl Into<String>,
        enabled: bool,
    ) -> Self {
        Self {
            store: Store {
                root: root.into(),
                cluster_id,
                cluster_uid: cluster_uid.into(),
                enabled,
            },
        }
    }

    pub fn path(&self) -> PathBuf {
        self.store.path(DISCOVERY_FILE)
    }

    /// Save atomically. An unknown server version or disabled cache is a no-op.
    pub fn save(&self, catalog: &ResourceCatalog, server_version: &str) -> Result<(), CacheError> {
        if !self.store.active() || server_version.is_empty() {
            return Ok(());
        }
        let file = CatalogFile {
            version: FORMAT_VERSION,
            cluster_uid: self.store.cluster_uid.clone(),
            saved_at: unix_secs(SystemTime::now()),
            server_version: server_version.to_string(),
            catalog: catalog.clone(),
        };
        self.store.write(DISCOVERY_FILE, &file)
    }

    /// Load a catalog. Invalid data is removed and returns `None`.
    pub fn load(&self, server_version: &str) -> Option<CachedCatalog> {
        if server_version.is_empty() {
            return None;
        }
        let file: CatalogFile = self.store.read(DISCOVERY_FILE)?;
        if file.version != FORMAT_VERSION
            || file.server_version != server_version
            || file.cluster_uid != self.store.cluster_uid
        {
            self.store.remove(DISCOVERY_FILE);
            return None;
        }
        let Some(saved_at) = from_unix_secs(file.saved_at) else {
            self.store.remove(DISCOVERY_FILE);
            return None;
        };
        Some(CachedCatalog {
            catalog: file.catalog,
            saved_at,
            stale: is_stale(saved_at, SystemTime::now()),
        })
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn from_unix_secs(secs: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Treat future timestamps as fresh to tolerate clock changes.
fn is_stale(saved_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(saved_at)
        .is_ok_and(|age| age > CACHE_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    const UID: &str = "cluster-uid-1";

    fn temp_root(name: &str) -> TempRoot {
        TempRoot::new(name)
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("k8s-gpui-cache-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn cluster_id() -> ClusterId {
        ClusterId::derive("ctx", "https://api.example.com:6443")
    }

    fn snapshot(root: &Path, uid: &str, enabled: bool) -> SnapshotCache {
        SnapshotCache::new(root, cluster_id(), uid, enabled)
    }

    fn pods() -> GvrKey {
        GvrKey::new("", "v1", "pods")
    }

    fn object(kind: &str, api_version: &str, name: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": api_version,
                "kind": kind,
                "metadata": { "name": name, "uid": format!("uid-{name}") },
                "spec": { "replicas": 1 },
            }))
            .expect("synthetic object"),
        )
    }

    fn names(objects: &[Arc<DynamicObject>]) -> Vec<&str> {
        objects
            .iter()
            .filter_map(|obj| obj.metadata.name.as_deref())
            .collect()
    }

    #[test]
    fn secret_gvr_is_never_written() {
        let root = temp_root("secret-gvr");
        let cache = snapshot(&root.path(), UID, true);
        let secrets = GvrKey::new("", "v1", "secrets");
        cache
            .save(&secrets, &[object("Secret", "v1", "top-secret")])
            .expect("Secret GVR is skipped");
        assert!(
            !cache.path(&secrets).exists(),
            "Secret GVR must not create a file"
        );
        assert!(cache.load(&secrets).is_none());
    }

    #[test]
    fn secret_purge_ignores_cache_state_without_removing_other_entries() {
        let root = temp_root("secret-purge");
        let active = snapshot(&root.path(), UID, true);
        let secrets = GvrKey::new("", "v1", "secrets");
        let secret_path = active.path(&secrets);
        let pods_path = active.path(&pods());
        fs::create_dir_all(secret_path.parent().expect("parent directory"))
            .expect("create directory");
        fs::write(&secret_path, b"historical secret snapshot").expect("write historical cache");
        active
            .save(&pods(), &[object("Pod", "v1", "alpha")])
            .expect("save pods");

        for cache in [
            snapshot(&root.path(), UID, false),
            snapshot(&root.path(), "", true),
        ] {
            cache.save(&secrets, &[]).expect("purge secrets");
            assert!(!secret_path.exists());
            assert!(pods_path.exists());
        }
    }

    #[test]
    fn aggregate_snapshot_drops_secret_objects_before_write() {
        let root = temp_root("aggregate");
        let cache = snapshot(&root.path(), UID, true);
        let objects = vec![
            object("Deployment", "apps/v1", "web"),
            object("Secret", "v1", "top-secret"),
            object("ConfigMap", "v1", "settings"),
        ];
        let gvr = GvrKey::new("apps", "v1", "deployments");
        cache.save(&gvr, &objects).expect("save to disk");

        let raw = fs::read_to_string(cache.path(&gvr)).expect("file exists");
        assert!(
            !raw.contains("top-secret"),
            "Secret names must not reach disk"
        );
        assert!(
            !raw.contains("\"Secret\""),
            "Secret kind must not reach disk"
        );

        let loaded = cache.load(&gvr).expect("cache hit");
        let kinds: Vec<&str> = loaded
            .objects
            .iter()
            .filter_map(|obj| obj.types.as_ref().map(|types| types.kind.as_str()))
            .collect();
        assert_eq!(kinds, ["Deployment", "ConfigMap"]);
    }

    #[test]
    fn load_defensively_drops_secret_objects_from_disk() {
        let root = temp_root("load-defense");
        let cache = snapshot(&root.path(), UID, true);
        let gvr = pods();
        let path = cache.path(&gvr);
        fs::create_dir_all(path.parent().expect("parent directory")).expect("create directory");
        let file = json!({
            "version": FORMAT_VERSION,
            "cluster_uid": UID,
            "saved_at": unix_secs(SystemTime::now()),
            "gvr": { "group": "", "version": "v1", "plural": "pods" },
            "objects": [
                { "apiVersion": "v1", "kind": "Pod", "metadata": { "name": "alpha" } },
                { "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "leak" } },
                { "apiVersion": "v1", "kind": "secret", "metadata": { "name": "leak-2" } },
            ],
        });
        fs::write(&path, serde_json::to_vec(&file).expect("serialize")).expect("write file");

        let loaded = cache.load(&gvr).expect("valid structure hits cache");
        assert_eq!(names(&loaded.objects), ["alpha"]);
    }

    #[test]
    fn concurrent_saves_leave_valid_json_and_no_temp_files() {
        let root = temp_root("atomic");
        let cache = Arc::new(snapshot(&root.path(), UID, true));
        let gvr = pods();

        std::thread::scope(|scope| {
            for index in 0..8 {
                let cache = Arc::clone(&cache);
                let gvr = gvr.clone();
                scope.spawn(move || {
                    let objects = vec![object("Pod", "v1", &format!("pod-{index}"))];
                    cache.save(&gvr, &objects).expect("concurrent save to disk");
                });
            }
        });

        let loaded = cache.load(&gvr).expect("cache hit after concurrent writes");
        assert_eq!(loaded.objects.len(), 1, "rename leaves one complete write");
        let entries: Vec<String> = fs::read_dir(root.path().join(cluster_id().to_string()))
            .expect("cluster directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(entries, ["core.v1.pods.json"], "no temporary files remain");
    }

    #[cfg(unix)]
    #[test]
    fn cache_directories_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("permissions");
        fs::create_dir_all(root.path()).expect("create a wide-permission directory");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))
            .expect("relax permissions");

        let cache = snapshot(&root.path(), UID, true);
        cache
            .save(&pods(), &[object("Pod", "v1", "alpha")])
            .expect("save to disk");

        let root_mode = fs::metadata(root.path())
            .expect("cache root")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700, "the cache root must use mode 0700");
        let dir_mode = fs::metadata(root.path().join(cluster_id().to_string()))
            .expect("cluster directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the cluster directory must use mode 0700");
        let file_mode = fs::metadata(cache.path(&pods()))
            .expect("cache file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "the cache file must use mode 0600");
    }
}
