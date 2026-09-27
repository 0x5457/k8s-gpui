//! Builds disk cache handles from the user setting and cluster UID.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use k8s_core::cache::{DiscoveryCache, GvrKey, SnapshotCache};
use k8s_core::cluster::{ClusterId, ClusterRegistry};
use k8s_core::cluster_data::{ClusterDataSource, data_source_for_cluster};
use k8s_core::discovery::ResourceCatalog;
use k8s_core::paths::cache_file;
use kube_core::DynamicObject;
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;

pub use crate::session::ClusterCache;
pub use crate::settings::user_settings_path;

/// Limits cluster UID lookup. A timeout leaves the UID unknown.
pub const UID_TIMEOUT: Duration = Duration::from_secs(2);

/// Defines the `diskCache` setting key. The cache defaults to enabled.
pub const DISK_CACHE_KEY: &str = "diskCache";

const WRITE_COALESCE_DELAY: Duration = Duration::from_millis(10);
const MAX_WRITE_STATES: usize = 256;

static WRITE_STATES: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<WriteSlot>>>>> =
    OnceLock::new();
static WRITE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn write_state(path: &Path) -> Arc<AsyncMutex<WriteSlot>> {
    let mut states = WRITE_STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    states.retain(|_, state| state.strong_count() > 0);
    if let Some(state) = states.get(path).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(AsyncMutex::new(WriteSlot::default()));
    if states.len() < MAX_WRITE_STATES {
        states.insert(path.to_path_buf(), Arc::downgrade(&state));
    }
    state
}

fn remove_write_state_if_idle(path: &Path, state: &Arc<AsyncMutex<WriteSlot>>) {
    if Arc::strong_count(state) != 1 {
        return;
    }
    let mut states = WRITE_STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if Arc::strong_count(state) == 1
        && states
            .get(path)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, state))
    {
        states.remove(path);
    }
}

pub(crate) fn next_cache_write_generation() -> u64 {
    WRITE_GENERATION.fetch_add(1, Ordering::Relaxed)
}

enum CacheWrite {
    Snapshot {
        generation: u64,
        store: SnapshotCache,
        gvr: GvrKey,
        objects: Vec<Arc<DynamicObject>>,
    },
    Discovery {
        generation: u64,
        store: DiscoveryCache,
        catalog: ResourceCatalog,
        version: String,
    },
}

impl CacheWrite {
    fn generation(&self) -> u64 {
        match self {
            Self::Snapshot { generation, .. } | Self::Discovery { generation, .. } => *generation,
        }
    }

    fn path(&self) -> PathBuf {
        match self {
            Self::Snapshot { store, gvr, .. } => store.path(gvr),
            Self::Discovery { store, .. } => store.path(),
        }
    }

    async fn write(self) {
        match self {
            Self::Snapshot {
                store,
                gvr,
                objects,
                ..
            } => {
                let _ = tokio::task::spawn_blocking(move || store.save(&gvr, &objects)).await;
            }
            Self::Discovery {
                store,
                catalog,
                version,
                ..
            } => {
                let _ = tokio::task::spawn_blocking(move || store.save(&catalog, &version)).await;
            }
        }
    }
}

#[derive(Default)]
struct WriteSlot {
    latest_generation: u64,
    writing: bool,
    pending: Option<CacheWrite>,
}

impl WriteSlot {
    fn enqueue(&mut self, write: CacheWrite) -> bool {
        let generation = write.generation();
        if generation < self.latest_generation {
            return false;
        }
        self.latest_generation = generation;
        self.pending = Some(write);
        if self.writing {
            false
        } else {
            self.writing = true;
            true
        }
    }

    fn take(&mut self) -> Option<CacheWrite> {
        self.pending.take()
    }
}

async fn drain_cache_writes(path: PathBuf, state: Arc<AsyncMutex<WriteSlot>>) {
    tokio::time::sleep(WRITE_COALESCE_DELAY).await;
    loop {
        let write = {
            let mut slot = state.lock().await;
            match slot.take() {
                Some(write) => Some(write),
                None => {
                    slot.writing = false;
                    None
                }
            }
        };
        let Some(write) = write else {
            remove_write_state_if_idle(&path, &state);
            return;
        };
        write.write().await;
    }
}

async fn enqueue_cache_write(handle: &Handle, write: CacheWrite) {
    let path = write.path();
    let state = write_state(&path);
    if state.lock().await.enqueue(write) {
        handle.spawn(drain_cache_writes(path, state));
    } else {
        remove_write_state_if_idle(&path, &state);
    }
}

pub(crate) async fn enqueue_snapshot_write(
    handle: &Handle,
    generation: u64,
    store: SnapshotCache,
    gvr: GvrKey,
    objects: Vec<Arc<DynamicObject>>,
) {
    enqueue_cache_write(
        handle,
        CacheWrite::Snapshot {
            generation,
            store,
            gvr,
            objects,
        },
    )
    .await;
}

pub(crate) async fn enqueue_discovery_write(
    handle: &Handle,
    store: DiscoveryCache,
    catalog: ResourceCatalog,
    version: String,
) {
    enqueue_cache_write(
        handle,
        CacheWrite::Discovery {
            generation: next_cache_write_generation(),
            store,
            catalog,
            version,
        },
    )
    .await;
}

/// Parses the cache setting and defaults to enabled for invalid values.
pub fn cache_enabled_in(text: &str) -> bool {
    let Ok(value) = settings::parse_json_with_comments::<serde_json::Value>(text) else {
        return true;
    };
    value
        .get(DISK_CACHE_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Reads the cache setting and defaults to enabled when the file is absent.
pub fn disk_cache_enabled() -> bool {
    let Some(path) = user_settings_path() else {
        return true;
    };
    match std::fs::read_to_string(path) {
        Ok(text) => cache_enabled_in(&text),
        Err(_) => true,
    }
}

impl std::fmt::Debug for ClusterCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterCache")
            .field("cluster_id", &self.cluster_id)
            .field("uid_resolved", &self.uid.get().is_some())
            .finish_non_exhaustive()
    }
}

impl ClusterCache {
    /// Creates a handle when the setting and cache root are available.
    pub fn from_settings(registry: &ClusterRegistry, cluster_id: ClusterId) -> Option<Arc<Self>> {
        Self::from_settings_with_service(registry, cluster_id, None)
    }

    pub fn from_settings_with_service(
        registry: &ClusterRegistry,
        cluster_id: ClusterId,
        service: Option<ClusterDataSource>,
    ) -> Option<Arc<Self>> {
        if !disk_cache_enabled() {
            return None;
        }
        let root = cache_file("")?;
        Some(Arc::new(Self {
            root,
            cluster_id,
            enabled: true,
            uid: registry.uid_cell(cluster_id),
            service,
        }))
    }

    pub fn cluster_id(&self) -> ClusterId {
        self.cluster_id
    }

    /// Starts an early cluster UID lookup.
    pub fn spawn_uid_warm(&self, registry: Arc<ClusterRegistry>, handle: &tokio::runtime::Handle) {
        if self.uid.get().is_some() {
            return;
        }
        let cache = self.clone();
        handle.spawn(async move {
            cache.uid(&registry).await;
        });
    }

    /// Resolves and caches the cluster UID. Returns `None` on failure.
    pub async fn uid(&self, registry: &Arc<ClusterRegistry>) -> Option<String> {
        let service = self
            .service
            .clone()
            .or_else(|| data_source_for_cluster(registry, self.cluster_id))?;
        self.uid
            .get_or_try_init(|| {
                let service = service.clone();
                async move { fetch_cluster_uid(&service).await }
            })
            .await
            .ok()
            .cloned()
    }

    /// Returns a snapshot cache when the cluster UID is known.
    pub async fn snapshot(&self, registry: &Arc<ClusterRegistry>) -> Option<SnapshotCache> {
        let uid = self.uid(registry).await?;
        Some(SnapshotCache::new(
            &self.root,
            self.cluster_id,
            uid,
            self.enabled,
        ))
    }

    /// Returns a discovery cache when the cluster UID is known.
    pub async fn discovery(&self, registry: &Arc<ClusterRegistry>) -> Option<DiscoveryCache> {
        let uid = self.uid(registry).await?;
        Some(DiscoveryCache::new(
            &self.root,
            self.cluster_id,
            uid,
            self.enabled,
        ))
    }

    /// Returns the cache root path.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

async fn fetch_cluster_uid(service: &ClusterDataSource) -> Result<String, String> {
    let port = service.port();
    let namespace = tokio::time::timeout(UID_TIMEOUT, port.cluster_uid())
        .await
        .map_err(|_| "timed out resolving the cluster UID".to_owned())??;
    Ok(namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_core::cluster_data::{ClusterDataPort, DataFuture};
    use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric};
    use k8s_core::overview::Overview;
    use std::sync::atomic::AtomicUsize;

    fn cache_write(generation: u64, name: &str) -> CacheWrite {
        let root = std::env::temp_dir().join(format!(
            "k8s-gpui-write-slot-{}-{generation}-{name}",
            std::process::id()
        ));
        let object: Arc<DynamicObject> = Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": { "name": name, "uid": name },
            }))
            .expect("synthetic pod"),
        );
        CacheWrite::Snapshot {
            generation,
            store: SnapshotCache::new(
                root,
                ClusterId::derive("write-slot", "https://write-slot.example.com"),
                "uid",
                true,
            ),
            gvr: GvrKey::new("", "v1", "pods"),
            objects: vec![object],
        }
    }

    struct TestPort {
        uid: Arc<Mutex<Option<String>>>,
        gets: Arc<AtomicUsize>,
    }

    impl ClusterDataPort for TestPort {
        fn overview(&self, _metrics: bool) -> DataFuture<Overview, String> {
            Box::pin(async { Ok(Overview::default()) })
        }

        fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
            Box::pin(async { Ok(()) })
        }

        fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn metrics_pods(
            &self,
            _namespace: Option<String>,
        ) -> DataFuture<Vec<PodMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn namespaces(&self) -> DataFuture<Vec<String>, String> {
            Box::pin(async { Ok(vec!["default".to_owned(), "kube-system".to_owned()]) })
        }

        fn cluster_uid(&self) -> DataFuture<String, String> {
            let uid = self
                .uid
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let gets = Arc::clone(&self.gets);
            Box::pin(async move {
                gets.fetch_add(1, Ordering::Relaxed);
                uid.ok_or_else(|| "UID unavailable".to_owned())
            })
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok("v1.36.0".to_owned()) })
        }
    }

    fn test_source(uid: &Arc<Mutex<Option<String>>>, gets: &Arc<AtomicUsize>) -> ClusterDataSource {
        ClusterDataSource::from_port(Arc::new(TestPort {
            uid: Arc::clone(uid),
            gets: Arc::clone(gets),
        }))
    }

    fn test_cache(
        registry: &ClusterRegistry,
        cluster_id: ClusterId,
        service: ClusterDataSource,
    ) -> Arc<ClusterCache> {
        Arc::new(ClusterCache {
            root: std::env::temp_dir().join("k8s-gpui-cache-uid-test"),
            cluster_id,
            enabled: true,
            uid: registry.uid_cell(cluster_id),
            service: Some(service),
        })
    }

    #[tokio::test]
    async fn uid_is_shared_within_registry_and_new_registry_rechecks() {
        let cluster = ClusterId::derive("uid-shared", "https://uid-shared.example.com");
        let registry = Arc::new(ClusterRegistry::default());
        let uid = Arc::new(Mutex::new(Some("uid-before".to_owned())));
        let gets = Arc::new(AtomicUsize::new(0));
        let first = test_cache(&registry, cluster, test_source(&uid, &gets));
        let second = test_cache(&registry, cluster, test_source(&uid, &gets));

        let (first_uid, second_uid) = tokio::join!(first.uid(&registry), second.uid(&registry));
        assert_eq!(first_uid.as_deref(), Some("uid-before"));
        assert_eq!(second_uid.as_deref(), Some("uid-before"));
        assert_eq!(gets.load(Ordering::Relaxed), 1);

        *uid.lock().unwrap_or_else(|error| error.into_inner()) = Some("uid-after".to_owned());
        assert_eq!(first.uid(&registry).await.as_deref(), Some("uid-before"));
        assert_eq!(second.uid(&registry).await.as_deref(), Some("uid-before"));
        assert_eq!(gets.load(Ordering::Relaxed), 1);

        let reloaded = Arc::new(ClusterRegistry::default());
        let reloaded_cache = test_cache(&reloaded, cluster, test_source(&uid, &gets));
        assert_eq!(
            reloaded_cache.uid(&reloaded).await.as_deref(),
            Some("uid-after")
        );
        assert_eq!(gets.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn uid_failure_stays_fail_closed_and_can_retry() {
        let cluster = ClusterId::derive("uid-failure", "https://uid-failure.example.com");
        let registry = Arc::new(ClusterRegistry::default());
        let uid = Arc::new(Mutex::new(None));
        let gets = Arc::new(AtomicUsize::new(0));
        let cache = test_cache(&registry, cluster, test_source(&uid, &gets));

        assert!(cache.snapshot(&registry).await.is_none());
        assert!(cache.discovery(&registry).await.is_none());
        assert_eq!(gets.load(Ordering::Relaxed), 2);
        *uid.lock().unwrap_or_else(|error| error.into_inner()) = Some("uid-recovered".to_owned());
        assert_eq!(cache.uid(&registry).await.as_deref(), Some("uid-recovered"));
        assert_eq!(gets.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn cache_writes_coalesce_to_the_latest_generation() {
        let mut slot = WriteSlot::default();

        assert!(slot.enqueue(cache_write(1, "old")));
        assert!(!slot.enqueue(cache_write(2, "middle")));
        assert!(!slot.enqueue(cache_write(3, "new")));
        assert!(!slot.enqueue(cache_write(2, "late-old")));

        match slot.take() {
            Some(CacheWrite::Snapshot {
                generation,
                objects,
                ..
            }) => {
                assert_eq!(generation, 3);
                assert_eq!(
                    objects[0].metadata.name.as_deref(),
                    Some("new"),
                    "only the newest pending snapshot must run"
                );
            }
            _ => panic!("expected latest snapshot"),
        }
        assert!(slot.take().is_none());
        slot.writing = false;
        assert!(!slot.enqueue(cache_write(1, "completed-old")));
    }

    fn write_state_key_present(path: &Path) -> bool {
        WRITE_STATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(path)
    }

    #[tokio::test]
    async fn cache_write_slots_are_reclaimed_after_drain() {
        let generation = next_cache_write_generation();
        let write = cache_write(generation, "reclaim");
        let path = write.path();
        let root = path
            .parent()
            .and_then(Path::parent)
            .expect("cache path must have a root");
        enqueue_cache_write(&Handle::current(), write).await;

        tokio::time::timeout(Duration::from_secs(5), async {
            while write_state_key_present(&path) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cache write slot must be reclaimed");

        assert!(!write_state_key_present(&path));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_is_on_by_default_and_off_when_disabled() {
        assert!(cache_enabled_in(""));
        assert!(cache_enabled_in("{}"));
        assert!(cache_enabled_in(r#"{"theme": "One Dark"}"#));
        assert!(!cache_enabled_in(r#"{"diskCache": false}"#));
        assert!(cache_enabled_in(r#"{"diskCache": true}"#));
        assert!(cache_enabled_in(r#"{"diskCache": "nope"}"#));
        assert!(
            cache_enabled_in("{not json"),
            "Invalid files do not block startup"
        );
    }

    #[test]
    fn cache_enabled_tolerates_jsonc_comments() {
        assert!(!cache_enabled_in(
            "{\n  // Prefer cross-region performance\n  \"diskCache\": false,\n}"
        ));
    }
}
