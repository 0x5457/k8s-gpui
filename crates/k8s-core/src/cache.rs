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
        self.store.path(&format!("{}.json", gvr.file_stem()))
    }

    /// Save atomically. Secret GVRs are skipped and old files are removed.
    pub fn save(&self, gvr: &GvrKey, objects: &[Arc<DynamicObject>]) -> Result<(), CacheError> {
        let file_name = format!("{}.json", gvr.file_stem());
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
        let file_name = format!("{}.json", gvr.file_stem());
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

    fn discovery(root: &Path, uid: &str, enabled: bool) -> DiscoveryCache {
        DiscoveryCache::new(root, cluster_id(), uid, enabled)
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

    fn edit_file(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("read cache file")).expect("valid JSON");
        edit(&mut value);
        fs::write(path, serde_json::to_vec(&value).expect("serialize")).expect("write cache file");
    }

    fn rewrite_saved_at(path: &Path, secs_ago: u64) {
        edit_file(path, |value| {
            value["saved_at"] = json!(unix_secs(SystemTime::now()) - secs_ago);
        });
    }

    fn catalog() -> ResourceCatalog {
        serde_json::from_value(json!({
            "groups": [{
                "group": "",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [{
                        "group": "",
                        "version": "v1",
                        "kind": "Pod",
                        "plural": "pods",
                        "scope": "namespaced",
                        "verbs": ["get", "list", "watch"],
                    }],
                }],
            }],
        }))
        .expect("synthetic catalog")
    }

    #[test]
    fn cache_path_is_cluster_dir_plus_gvr() {
        let root = temp_root("path");
        let cache = snapshot(&root.path(), UID, true);
        let cluster_dir = root.path().join(cluster_id().to_string());

        assert_eq!(
            cache.path(&GvrKey::new("apps", "v1", "deployments")),
            cluster_dir.join("apps.v1.deployments.json")
        );
        assert_eq!(cache.path(&pods()), cluster_dir.join("core.v1.pods.json"));
    }

    #[test]
    fn gvr_file_stem_maps_core_and_sanitizes() {
        assert_eq!(GvrKey::new("", "v1", "pods").file_stem(), "core.v1.pods");
        assert_eq!(
            GvrKey::new("apps", "v1", "deployments").file_stem(),
            "apps.v1.deployments"
        );
        assert_eq!(
            GvrKey::new("example.com", "v1beta1", "widgets").file_stem(),
            "example.com.v1beta1.widgets"
        );

        let root = temp_root("sanitize");
        let cache = snapshot(&root.path(), UID, true);
        let cluster_dir = root.path().join(cluster_id().to_string());
        let evil = cache.path(&GvrKey::new("../../etc", "v1", "pods"));
        assert!(
            !evil
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .contains('/')
        );
        assert_eq!(evil.parent(), Some(cluster_dir.as_path()));
    }

    #[test]
    fn round_trips_objects_and_marks_fresh() {
        let root = temp_root("round-trip");
        let cache = snapshot(&root.path(), UID, true);
        let objects = vec![object("Pod", "v1", "alpha"), object("Pod", "v1", "beta")];
        cache.save(&pods(), &objects).expect("save to disk");

        let loaded = cache.load(&pods()).expect("cache hit");
        assert!(!loaded.stale, "a new snapshot is not stale");
        assert_eq!(names(&loaded.objects), ["alpha", "beta"]);
        assert!(
            SystemTime::now()
                .duration_since(loaded.saved_at)
                .expect("saved_at is not in the future")
                < Duration::from_secs(60)
        );
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
    fn secret_gvr_predicate_is_case_insensitive_and_accepts_core_alias() {
        assert!(is_secret_gvr(&GvrKey::new("", "v1", "secrets")));
        assert!(is_secret_gvr(&GvrKey::new("", "v1", "Secrets")));
        assert!(is_secret_gvr(&GvrKey::new("", "v1", "SECRETS")));
        assert!(is_secret_gvr(&GvrKey::new("core", "v1", "secrets")));
        assert!(!is_secret_gvr(&GvrKey::new("", "v1", "configmaps")));
        assert!(
            !is_secret_gvr(&GvrKey::new("apps", "v1", "secrets")),
            "a CRD in a non-core group is not excluded by GVR, but object-kind filtering still applies"
        );
    }

    #[test]
    fn secret_object_predicate_is_case_insensitive() {
        assert!(is_secret_object(&object("Secret", "v1", "a")));
        assert!(is_secret_object(&object("secret", "v1", "b")));
        assert!(is_secret_object(&object("SECRET", "v1", "c")));
        assert!(!is_secret_object(&object("ConfigMap", "v1", "d")));
        let bare: DynamicObject =
            serde_json::from_value(json!({ "metadata": { "name": "no-types" } }))
                .expect("no types");
        assert!(!is_secret_object(&bare));
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
    fn ttl_boundary_marks_only_expired_snapshots_stale() {
        let root = temp_root("ttl");
        let cache = snapshot(&root.path(), UID, true);
        let gvr = pods();
        cache
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        let path = cache.path(&gvr);

        rewrite_saved_at(&path, CACHE_TTL.as_secs() - 60);
        let fresh = cache.load(&gvr).expect("unexpired cache hit");
        assert!(!fresh.stale);

        rewrite_saved_at(&path, CACHE_TTL.as_secs() + 60);
        let expired = cache
            .load(&gvr)
            .expect("expired data remains available for immediate display");
        assert!(expired.stale, "entries older than the TTL are marked stale");
    }

    #[test]
    fn cluster_uid_mismatch_is_a_miss_and_removes_file() {
        let root = temp_root("uid");
        let writer = snapshot(&root.path(), "uid-a", true);
        let gvr = pods();
        writer
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        let path = writer.path(&gvr);
        assert!(path.exists());

        let reader = snapshot(&root.path(), "uid-b", true);
        assert!(
            reader.load(&gvr).is_none(),
            "the old cache is invalid after cluster rebuild"
        );
        assert!(!path.exists(), "invalid cache files are removed");
    }

    #[test]
    fn format_or_gvr_mismatch_is_a_miss() {
        let root = temp_root("format");
        let cache = snapshot(&root.path(), UID, true);
        let gvr = pods();
        cache
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        let path = cache.path(&gvr);

        edit_file(&path, |file| file["version"] = json!(FORMAT_VERSION + 1));
        assert!(
            cache.load(&gvr).is_none(),
            "format version mismatch is a cache miss"
        );
        assert!(!path.exists());

        cache
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        edit_file(&path, |file| {
            file["gvr"] = json!({ "group": "apps", "version": "v1", "plural": "deployments" });
        });
        assert!(
            cache.load(&gvr).is_none(),
            "file GVR differs from the request"
        );
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_or_truncated_file_is_a_miss_and_removed() {
        let root = temp_root("corrupt");
        let cache = snapshot(&root.path(), UID, true);
        let gvr = pods();
        let path = cache.path(&gvr);
        fs::create_dir_all(path.parent().expect("parent directory")).expect("create directory");

        fs::write(&path, b"{not json").expect("write invalid file");
        assert!(cache.load(&gvr).is_none());
        assert!(!path.exists(), "invalid JSON is removed");

        cache
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        let bytes = fs::read(&path).expect("read file");
        fs::write(&path, &bytes[..bytes.len() / 2]).expect("truncate file");
        assert!(cache.load(&gvr).is_none(), "truncated file is a cache miss");
        assert!(!path.exists(), "truncated file is removed");
    }

    #[test]
    fn overflowing_saved_at_is_a_miss_and_removed() {
        let root = temp_root("saved-at-overflow");
        let cache = snapshot(&root.path(), UID, true);
        let gvr = pods();
        cache
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("save to disk");
        let path = cache.path(&gvr);
        edit_file(&path, |file| file["saved_at"] = json!(u64::MAX));

        assert!(cache.load(&gvr).is_none());
        assert!(!path.exists());
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

    #[test]
    fn disabled_or_uid_less_cache_does_no_io() {
        let root = temp_root("disabled");
        let gvr = pods();

        let disabled = snapshot(&root.path(), UID, false);
        disabled
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("no-op");
        assert!(!disabled.path(&gvr).exists());
        assert!(disabled.load(&gvr).is_none());

        let uid_less = snapshot(&root.path(), "", true);
        uid_less
            .save(&gvr, &[object("Pod", "v1", "alpha")])
            .expect("no-op");
        assert!(
            !uid_less.path(&gvr).exists(),
            "an unknown UID prevents cache writes"
        );
        assert!(uid_less.load(&gvr).is_none());
        assert!(
            !root.path().exists(),
            "a disabled cache does not create its directory"
        );
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

    #[test]
    fn discovery_round_trips() {
        let root = temp_root("discovery-round-trip");
        let cache = discovery(&root.path(), UID, true);
        let catalog = catalog();
        cache.save(&catalog, "v1.36.0").expect("save to disk");

        let loaded = cache.load("v1.36.0").expect("cache hit");
        assert_eq!(loaded.catalog, catalog);
        assert!(!loaded.stale);
        assert!(loaded.saved_at <= SystemTime::now());
    }

    #[test]
    fn discovery_version_or_uid_mismatch_is_a_miss() {
        let root = temp_root("discovery-mismatch");
        let cache = discovery(&root.path(), UID, true);
        cache.save(&catalog(), "v1.36.0").expect("save to disk");

        assert!(
            cache.load("v1.37.0").is_none(),
            "a server version change invalidates the cache"
        );
        assert!(
            !cache.path().exists(),
            "the invalid catalog cache is removed"
        );

        cache.save(&catalog(), "v1.36.0").expect("save to disk");
        let other = discovery(&root.path(), "uid-b", true);
        assert!(
            other.load("v1.36.0").is_none(),
            "a different cluster UID invalidates the cache"
        );
        assert!(!cache.path().exists());
    }

    #[test]
    fn discovery_ttl_expiry_marks_stale_and_corruption_recovers() {
        let root = temp_root("discovery-ttl");
        let cache = discovery(&root.path(), UID, true);
        cache.save(&catalog(), "v1.36.0").expect("save to disk");

        rewrite_saved_at(&cache.path(), CACHE_TTL.as_secs() + 60);
        let expired = cache
            .load("v1.36.0")
            .expect("expired data is still returned");
        assert!(expired.stale);

        fs::write(cache.path(), b"garbage").expect("write invalid file");
        assert!(cache.load("v1.36.0").is_none());
        assert!(
            !cache.path().exists(),
            "the invalid catalog cache is removed"
        );
    }

    #[test]
    fn discovery_disabled_or_version_unknown_does_no_io() {
        let root = temp_root("discovery-disabled");
        let disabled = discovery(&root.path(), UID, false);
        disabled.save(&catalog(), "v1.36.0").expect("no-op");
        assert!(!disabled.path().exists());
        assert!(disabled.load("v1.36.0").is_none());

        let cache = discovery(&root.path(), UID, true);
        cache
            .save(&catalog(), "")
            .expect("unknown version is a no-op");
        assert!(!cache.path().exists());
        assert!(cache.load("").is_none());
    }

    #[tokio::test]
    #[ignore = "Requires a kind cluster: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn kind_catalog_and_snapshot_round_trip() {
        use kube::ResourceExt;
        use kube::api::{Api, ListParams};

        if !crate::cluster::kubeconfig_present() {
            return;
        }
        let registry = crate::cluster::ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry.clusters().first() else {
            return;
        };

        let namespaces: Api<k8s_openapi::api::core::v1::Namespace> =
            Api::all(cluster.client().clone());
        let kube_system = namespaces.get("kube-system").await.expect("kube-system");
        let cluster_uid = kube_system.uid().expect("kube-system has a UID");
        let server_version = cluster
            .client()
            .apiserver_version()
            .await
            .expect("read /version")
            .git_version;

        let root = temp_root("kind");

        let catalog = ResourceCatalog::fetch(cluster)
            .await
            .expect("discovery succeeds");
        let discovery = discovery(&root.path(), &cluster_uid, true);
        discovery
            .save(&catalog, &server_version)
            .expect("save catalog to disk");
        let loaded = discovery.load(&server_version).expect("catalog cache hit");
        assert_eq!(loaded.catalog, catalog);

        let resource = ApiResource::from_gvk_with_plural(
            &kube::core::GroupVersionKind::gvk("", "v1", "Pod"),
            "pods",
        );
        let pods_api: Api<DynamicObject> = Api::all_with(cluster.client().clone(), &resource);
        let listed = pods_api
            .list(&ListParams::default())
            .await
            .expect("list Pods");
        let objects: Vec<Arc<DynamicObject>> = listed.items.into_iter().map(Arc::new).collect();
        let gvr = GvrKey::from_api_resource(&resource);
        let snapshots = snapshot(&root.path(), &cluster_uid, true);
        snapshots
            .save(&gvr, &objects)
            .expect("save snapshot to disk");
        let loaded = snapshots.load(&gvr).expect("snapshot cache hit");
        assert_eq!(loaded.objects.len(), objects.len());

        let secrets = GvrKey::new("", "v1", "secrets");
        snapshots
            .save(&secrets, &[object("Secret", "v1", "never")])
            .expect("Secret GVR no-op");
        assert!(!snapshots.path(&secrets).exists());
    }
}
