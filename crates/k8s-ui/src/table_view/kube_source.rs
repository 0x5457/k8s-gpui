//! Adapts Kubernetes controller events to the resource table source.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use k8s_core::cache::{GvrKey, SnapshotCache};
use k8s_core::cluster::{Client, Cluster, ClusterId, ClusterRegistry, Health};
use k8s_core::cluster_data::data_source_for_cluster;
use k8s_core::controller::{Controller, ControllerEvent, Scope, StoreEvent, StoreOp, WatchOptions};
use k8s_core::discovery::{ResourceCatalog, ResourceEntry, ResourceScope};
use k8s_core::hotbar::{Hotbar, HotbarError, cluster_index};
use k8s_core::latency::LatencyTier;
use k8s_core::ops;
use kube_core::{ApiResource, DynamicObject};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::session::{
    ApplyOutcome, ClusterSession, DescribeData, InspectorSource, LogEvent, LogFactory, LogRequest,
    LogSink, LogSubscription, ObjectRef, OpsFuture, ResolveOutcome, ServiceAccountTarget,
    pods_resource,
};

use super::cache::{
    ClusterCache, enqueue_discovery_write, enqueue_snapshot_write, next_cache_write_generation,
};
use super::host::SourceFactory;
use super::source::{ObjectOps, ResourceSource, SourceEvent, Subscription};

/// Limits the wait for the first complete resource list.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(30);

const CACHE_PRIME_BATCH_SIZE: usize = 256;
const CACHE_PRIME_BATCH_DELAY: Duration = Duration::from_millis(1);
const SNAPSHOT_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);
const SELECTED_CONTEXT_UNAVAILABLE: &str =
    "The selected context is no longer available. Reload Kubeconfigs and try again.";

/// Defines the Pod resource entry used by the compatibility factory.
fn pods_entry() -> ResourceEntry {
    ResourceEntry {
        group: String::new(),
        version: "v1".to_owned(),
        kind: "Pod".to_owned(),
        plural: "pods".to_owned(),
        scope: ResourceScope::Namespaced,
        verbs: Vec::new(),
    }
}

fn service_accounts_entry() -> ResourceEntry {
    ResourceEntry {
        group: String::new(),
        version: "v1".to_owned(),
        kind: "ServiceAccount".to_owned(),
        plural: "serviceaccounts".to_owned(),
        scope: ResourceScope::Namespaced,
        verbs: vec!["get".to_owned()],
    }
}

/// The discovery entry for whatever an [`ObjectRef`] names.
///
/// `resolve` has to issue the same `GET` a reader would, and `ops::read_resource`
/// is the only read path that carries the 30s timeout, the 404 → `ObjectNotFound`
/// mapping and the transport-error mapping — all three of which `resolve` needs
/// and none of which are worth reimplementing. It is keyed by a discovery entry
/// rather than an `ApiResource`, so one is built here.
///
/// The scope comes from the reference rather than from discovery, and that is not
/// a shortcut: a reference to a namespaced object always carries its namespace and
/// a reference to a cluster-scoped one never does, so the reference is the more
/// authoritative of the two when they disagree — and they only disagree when
/// discovery is stale, which is exactly the case where `read_resource`'s own
/// `entry.namespaced()` would silently turn a `GET /api/v1/nodes/x` into a
/// namespaced request and get a 404 that reads as "missing".
fn entry_for(object: &ObjectRef) -> ResourceEntry {
    ResourceEntry {
        group: object.resource.group.clone(),
        version: object.resource.version.clone(),
        kind: object.resource.kind.clone(),
        plural: object.resource.plural.clone(),
        scope: if object.namespace.is_some() {
            ResourceScope::Namespaced
        } else {
            ResourceScope::Cluster
        },
        verbs: Vec::new(),
    }
}

/// Turns one failed read into the answer it is entitled to be.
///
/// Only the two variants that mean "the API server answered, and there is no such
/// object" say [`ResolveOutcome::Missing`]. Everything else is
/// [`ResolveOutcome::Unknown`], and the two that are easiest to get wrong are
/// called out because they are the ones that would paint a panel full of
/// references as broken:
///
/// - **403/401** (`Api`) is a refusal, not a negative answer. The identity is not
///   allowed to know, and "does not know" is not "is not there".
/// - **`Transport` and `Timeout`** are a dead connection and an exhausted budget.
///   An offline session resolves every reference it is asked about at once, so
///   every one of them would come back `Missing` together.
///
/// `ResourceTypeNotFound` is the exception that is still `Missing`: the cluster
/// does not serve that type, so the object cannot exist. It cannot be reached from
/// a named read — `ops` maps a 404 on a named read to `ObjectNotFound` — and it is
/// listed rather than folded into `Unknown` so that stays true.
fn resolve_outcome_for(error: &ops::ResourceReadError) -> ResolveOutcome {
    match error {
        ops::ResourceReadError::ObjectNotFound { .. }
        | ops::ResourceReadError::ResourceTypeNotFound { .. } => ResolveOutcome::Missing,
        ops::ResourceReadError::Api { .. }
        | ops::ResourceReadError::Transport { .. }
        | ops::ResourceReadError::Timeout { .. } => ResolveOutcome::Unknown,
    }
}

/// Starts one Kubernetes watch for each subscription.
pub struct KubeSource {
    handle: Handle,
    registry: Arc<ClusterRegistry>,
    cluster: ClusterId,
    resource: ApiResource,
    scope: Scope,
    options: WatchOptions,
    init_timeout: Duration,
    /// Uses the disk cache only for `Scope::All`.
    cache: Option<Arc<ClusterCache>>,
}

impl KubeSource {
    pub fn new(
        handle: Handle,
        registry: Arc<ClusterRegistry>,
        cluster: ClusterId,
        resource: ApiResource,
        scope: Scope,
        options: WatchOptions,
    ) -> Self {
        Self {
            handle,
            registry,
            cluster,
            resource,
            scope,
            options,
            init_timeout: INIT_TIMEOUT,
            cache: None,
        }
    }

    /// Attaches an optional disk cache.
    pub fn with_cache(mut self, cache: Option<Arc<ClusterCache>>) -> Self {
        self.cache = cache;
        self
    }

    /// Creates a source for a discovered resource entry.
    pub fn for_entry(
        handle: Handle,
        registry: Arc<ClusterRegistry>,
        cluster: ClusterId,
        entry: &ResourceEntry,
        scope: Scope,
    ) -> Self {
        Self::new(
            handle,
            registry,
            cluster,
            entry.to_api_resource(),
            scope,
            WatchOptions::default(),
        )
    }

    /// Creates a Pod source for all namespaces.
    pub fn pods(handle: Handle, registry: Arc<ClusterRegistry>, cluster: ClusterId) -> Self {
        Self::pods_in(handle, registry, cluster, Scope::All)
    }

    /// Creates a Pod source for one scope.
    pub fn pods_in(
        handle: Handle,
        registry: Arc<ClusterRegistry>,
        cluster: ClusterId,
        scope: Scope,
    ) -> Self {
        Self::new(
            handle,
            registry,
            cluster,
            pods_resource(),
            scope,
            WatchOptions::default(),
        )
    }
}

struct ActiveController {
    _controller: Controller,
    events: mpsc::Receiver<ControllerEvent>,
}

enum EventSender {
    Bounded(mpsc::Sender<SourceEvent>),
    Unbounded(UnboundedSender<SourceEvent>),
}

impl EventSender {
    async fn send(&self, event: SourceEvent) -> bool {
        match self {
            Self::Bounded(events) => events.send(event).await.is_ok(),
            Self::Unbounded(events) => events.send(event).is_ok(),
        }
    }

    fn send_now(&self, event: SourceEvent) -> bool {
        match self {
            Self::Bounded(events) => events.try_send(event).is_ok(),
            Self::Unbounded(events) => events.send(event).is_ok(),
        }
    }
}

impl From<mpsc::Sender<SourceEvent>> for EventSender {
    fn from(events: mpsc::Sender<SourceEvent>) -> Self {
        Self::Bounded(events)
    }
}

impl From<UnboundedSender<SourceEvent>> for EventSender {
    fn from(events: UnboundedSender<SourceEvent>) -> Self {
        Self::Unbounded(events)
    }
}

enum SupervisorNext {
    Event(Option<ControllerEvent>),
    LatencyChanged,
    LatencyClosed,
    InitTimeout,
}

struct AbortOnDrop<T> {
    task: JoinHandle<T>,
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn join_abortable<T>(
    handle: &Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
{
    let mut task = AbortOnDrop {
        task: handle.spawn(future),
    };
    (&mut task.task).await
}

fn options_for_tier(base: &WatchOptions, tier: LatencyTier) -> WatchOptions {
    WatchOptions {
        label_selector: base.label_selector.clone(),
        field_selector: base.field_selector.clone(),
        tier,
    }
}

fn start_controller(
    client: &Client,
    resource: &ApiResource,
    scope: &Scope,
    options: &WatchOptions,
) -> Result<ActiveController, String> {
    Controller::spawn_with_events(
        client.clone(),
        resource.clone(),
        scope.clone(),
        options.clone(),
    )
    .map(|(controller, events)| ActiveController {
        _controller: controller,
        events,
    })
    .map_err(|error| {
        format!("Kubernetes updates did not start: {error}. Check cluster access and try again.")
    })
}

impl KubeSource {
    fn subscribe_with_sender(&self, events: EventSender) -> Box<dyn Subscription> {
        let Some(cluster) = self.registry.get(self.cluster) else {
            let _ = events.send_now(SourceEvent::Error {
                reason: SELECTED_CONTEXT_UNAVAILABLE.to_owned(),
            });
            return Box::new(NoopSubscription);
        };
        let client = cluster.client().clone();
        let mut latency = cluster.latency();
        let mut last_latency = cluster.latency_snapshot();
        let resource = self.resource.clone();
        let scope = self.scope.clone();
        let base_options = self.options.clone();
        let init_timeout = self.init_timeout;
        let registry = Arc::clone(&self.registry);
        let cache = self
            .cache
            .clone()
            .filter(|_| matches!(self.scope, Scope::All))
            .map(|cache| (cache, next_cache_write_generation()));
        let task = self.handle.spawn(async move {
            if !events
                .send(SourceEvent::LatencyUpdated(last_latency.clone()))
                .await
            {
                return;
            }
            let mut saver = match cache {
                Some((cache, generation)) => {
                    prime_from_cache(&cache, &registry, &resource, &events, generation).await
                }
                None => None,
            };
            let current_latency = latency.borrow_and_update().clone();
            if current_latency != last_latency {
                last_latency = current_latency;
                if !events
                    .send(SourceEvent::LatencyUpdated(last_latency.clone()))
                    .await
                {
                    return;
                }
            }
            let mut tier = last_latency.tier();
            let options = options_for_tier(&base_options, tier);
            let mut active = match start_controller(&client, &resource, &scope, &options) {
                Ok(active) => Some(active),
                Err(reason) => {
                    let _ = events.send(SourceEvent::Error { reason }).await;
                    return;
                }
            };
            let actual_tier = active
                .as_ref()
                .map(|active| active._controller.tier())
                .unwrap_or(tier);
            if !events
                .send(SourceEvent::ControllerTier(actual_tier))
                .await
            {
                return;
            }
            let mut init_done = false;
            let mut deadline = tokio::time::Instant::now() + init_timeout;
            loop {
                let current_latency = latency.borrow_and_update().clone();
                if current_latency != last_latency {
                    last_latency = current_latency.clone();
                    if !events
                        .send(SourceEvent::LatencyUpdated(last_latency.clone()))
                        .await
                    {
                        return;
                    }
                }
                let current_tier = last_latency.tier();
                if current_tier != tier {
                    if let Some(previous) = active.take() {
                        drop(previous);
                    }
                    let next = match start_controller(
                        &client,
                        &resource,
                        &scope,
                        &options_for_tier(&base_options, current_tier),
                    ) {
                        Ok(next) => next,
                        Err(reason) => {
                            let _ = events.send(SourceEvent::Error { reason }).await;
                            return;
                        }
                    };
                    tier = current_tier;
                    active = Some(next);
                    if !events
                        .send(SourceEvent::ControllerTier(
                            active.as_ref().expect("controller")._controller.tier(),
                        ))
                        .await
                    {
                        return;
                    }
                    init_done = false;
                    deadline = tokio::time::Instant::now() + init_timeout;
                    continue;
                }

                let next = if init_done {
                    tokio::select! {
                        event = active
                            .as_mut()
                            .expect("controller")
                            .events
                            .recv() => SupervisorNext::Event(event),
                        result = latency.changed() => {
                            if result.is_err() {
                                SupervisorNext::LatencyClosed
                            } else {
                                SupervisorNext::LatencyChanged
                            }
                        }
                    }
                } else {
                    match tokio::time::timeout_at(deadline, async {
                        tokio::select! {
                            event = active
                                .as_mut()
                                .expect("controller")
                                .events
                                .recv() => SupervisorNext::Event(event),
                            result = latency.changed() => {
                                if result.is_err() {
                                    SupervisorNext::LatencyClosed
                                } else {
                                    SupervisorNext::LatencyChanged
                                }
                            }
                        }
                    })
                    .await
                    {
                        Ok(next) => next,
                        Err(_) => SupervisorNext::InitTimeout,
                    }
                };

                match next {
                    SupervisorNext::LatencyChanged => continue,
                    SupervisorNext::LatencyClosed => {
                        let _ = events
                            .send(SourceEvent::Error {
                                reason:
                                    "The cluster latency monitor stopped. Refresh the view to reconnect."
                                        .to_owned(),
                            })
                            .await;
                        return;
                    }
                    SupervisorNext::InitTimeout => {
                        let _ = events
                            .send(SourceEvent::Error {
                                reason: initial_sync_timeout_reason(init_timeout),
                            })
                            .await;
                        return;
                    }
                    SupervisorNext::Event(None) => {
                        let _ = events
                            .send(SourceEvent::Error {
                                reason: "The connection for live updates ended. Refresh the view to reconnect."
                                    .to_owned(),
                            })
                            .await;
                        return;
                    }
                    SupervisorNext::Event(Some(event)) => {
                        let is_init = matches!(&event, ControllerEvent::Init);
                        let is_done = matches!(&event, ControllerEvent::InitDone);
                        if is_init {
                            // The budget is re-armed only for an init that follows a
                            // *finished* one, which is a reconnect of a source that
                            // was working.
                            //
                            // It used to be re-armed by every `Init`, and a failed
                            // initial list emits one on each attempt: `Controller`
                            // arms its init boundary again on every error and
                            // rebuilds. So an identity that is refused outright —
                            // RBAC, or a resource type the cluster does not serve —
                            // restarted the 30-second budget on every retry and the
                            // table sat on a skeleton with `Loading Pods…` for as
                            // long as the reader left it open. `§4.15`'s whole point
                            // is that a failure is stated where it happens; the one
                            // failure it cannot state is the one that never stops
                            // arriving.
                            //
                            // Re-arming after a completed init is the other half of
                            // the same rule and is kept: after `InitDone` the
                            // supervisor waits without a deadline, so a reconnect's
                            // `Init` has to install one or a dropped connection
                            // would hang with no bound at all.
                            if init_done {
                                deadline = tokio::time::Instant::now() + init_timeout;
                            }
                            init_done = false;
                        }
                        if !forward_controller_event(event, &events, &mut saver).await {
                            return;
                        }
                        if is_done {
                            init_done = true;
                        }
                    }
                }
            }
        });
        Box::new(KubeSubscription { task })
    }
}

impl ResourceSource for KubeSource {
    fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
        self.subscribe_with_sender(EventSender::Unbounded(events))
    }

    fn subscribe_bounded(&mut self, events: mpsc::Sender<SourceEvent>) -> Box<dyn Subscription> {
        self.subscribe_with_sender(EventSender::Bounded(events))
    }
}

/// Sends cached objects and returns a saver for the next live list.
async fn prime_from_cache(
    cache: &Arc<ClusterCache>,
    registry: &Arc<ClusterRegistry>,
    resource: &ApiResource,
    events: &EventSender,
    generation: u64,
) -> Option<SnapshotSaver> {
    let store = cache.snapshot(registry).await?;
    let gvr = GvrKey::from_api_resource(resource);
    let loader = store.clone();
    let load_gvr = gvr.clone();
    let snapshot = tokio::task::spawn_blocking(move || loader.load(&load_gvr))
        .await
        .ok()
        .flatten();
    if let Some(snapshot) = snapshot {
        if !events.send(SourceEvent::Init).await {
            return None;
        }
        for (index, object) in snapshot.objects.into_iter().enumerate() {
            let event = StoreEvent {
                op: StoreOp::Apply,
                obj: object,
            };
            if !events.send(SourceEvent::Store(event)).await {
                return None;
            }
            if (index + 1) % CACHE_PRIME_BATCH_SIZE == 0 {
                tokio::time::sleep(CACHE_PRIME_BATCH_DELAY).await;
            }
        }
        let _ = events
            .send(SourceEvent::CachePrimed {
                stale: snapshot.stale,
                saved_at: snapshot.saved_at,
            })
            .await;
    }
    Some(SnapshotSaver::new(store, gvr, generation))
}

/// Collects live objects and saves them after `InitDone`.
struct SnapshotSaver {
    store: SnapshotCache,
    gvr: GvrKey,
    objects: HashMap<String, Arc<DynamicObject>>,
    generation: u64,
    initialized: bool,
    pending_save: Option<AbortOnDrop<()>>,
}

impl SnapshotSaver {
    fn new(store: SnapshotCache, gvr: GvrKey, generation: u64) -> Self {
        Self {
            store,
            gvr,
            objects: HashMap::new(),
            generation,
            initialized: false,
            pending_save: None,
        }
    }

    fn begin(&mut self) {
        self.pending_save = None;
        self.generation = next_cache_write_generation();
        self.objects.clear();
        self.initialized = false;
    }

    fn apply(&mut self, event: &StoreEvent) {
        let Some(uid) = event.obj.metadata.uid.as_deref() else {
            return;
        };
        match event.op {
            StoreOp::Apply => {
                self.objects.insert(uid.to_owned(), Arc::clone(&event.obj));
            }
            StoreOp::Delete => {
                self.objects.remove(uid);
            }
        }
        if self.initialized {
            self.schedule_save();
        }
    }

    fn schedule_save(&mut self) {
        self.pending_save = None;
        let generation = next_cache_write_generation();
        self.generation = generation;
        let store = self.store.clone();
        let gvr = self.gvr.clone();
        let objects: Vec<Arc<DynamicObject>> = self.objects.values().cloned().collect();
        self.pending_save = Some(AbortOnDrop {
            task: tokio::spawn(async move {
                tokio::time::sleep(SNAPSHOT_SAVE_DEBOUNCE).await;
                enqueue_snapshot_write(&Handle::current(), generation, store, gvr, objects).await;
            }),
        });
    }

    async fn save(&mut self) {
        self.pending_save = None;
        let generation = self.generation;
        self.generation = next_cache_write_generation();
        let objects: Vec<Arc<DynamicObject>> = self.objects.values().cloned().collect();
        enqueue_snapshot_write(
            &Handle::current(),
            generation,
            self.store.clone(),
            self.gvr.clone(),
            objects,
        )
        .await;
    }

    async fn finish_init(&mut self) {
        self.initialized = true;
        self.save().await;
    }
}

/// Maps one controller event to one source event.
fn map_controller_event(event: ControllerEvent) -> SourceEvent {
    match event {
        ControllerEvent::Init => SourceEvent::Init,
        ControllerEvent::InitApply(obj) => SourceEvent::Store(StoreEvent {
            op: StoreOp::Apply,
            obj,
        }),
        ControllerEvent::InitDone => SourceEvent::InitDone,
        ControllerEvent::Store(event) => SourceEvent::Store(event),
    }
}

async fn forward_controller_event(
    event: ControllerEvent,
    events: &EventSender,
    saver: &mut Option<SnapshotSaver>,
) -> bool {
    let is_init = matches!(&event, ControllerEvent::Init);
    let is_done = matches!(&event, ControllerEvent::InitDone);
    if is_init && let Some(saver) = saver.as_mut() {
        saver.begin();
    }
    if let Some(saver) = saver.as_mut() {
        match &event {
            ControllerEvent::InitApply(object) => saver.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: Arc::clone(object),
            }),
            ControllerEvent::Store(store_event) => saver.apply(store_event),
            _ => {}
        }
    }
    if is_done && let Some(saver) = saver.as_mut() {
        saver.finish_init().await;
    }
    events.send(map_controller_event(event)).await
}

#[cfg(test)]
async fn forward_events(
    mut controller_events: mpsc::Receiver<ControllerEvent>,
    events: impl Into<EventSender>,
    init_timeout: Duration,
    mut saver: Option<SnapshotSaver>,
) {
    let events = events.into();
    let mut deadline = tokio::time::Instant::now() + init_timeout;
    let mut init_done = false;
    loop {
        let next = if init_done {
            controller_events.recv().await
        } else {
            match tokio::time::timeout_at(deadline, controller_events.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    let _ = events
                        .send(SourceEvent::Error {
                            reason: initial_sync_timeout_reason(init_timeout),
                        })
                        .await;
                    return;
                }
            }
        };
        let Some(event) = next else {
            let _ = events
                .send(SourceEvent::Error {
                    reason: "The connection for live updates ended. Refresh the view to reconnect."
                        .to_owned(),
                })
                .await;
            return;
        };
        let is_init = matches!(&event, ControllerEvent::Init);
        let is_done = matches!(&event, ControllerEvent::InitDone);
        if is_init {
            // Only an init that follows a completed one re-arms the budget; see the
            // supervisor's copy of this comment for why a retry storm must not.
            if init_done {
                deadline = tokio::time::Instant::now() + init_timeout;
            }
            init_done = false;
        }
        if !forward_controller_event(event, &events, &mut saver).await {
            return;
        }
        if is_done {
            init_done = true;
        }
    }
}

fn initial_sync_timeout_reason(init_timeout: Duration) -> String {
    format!(
        "Initial data load did not finish within {}s. Check the cluster connection and access permissions, then retry.",
        init_timeout.as_secs().max(1)
    )
}

struct KubeSubscription {
    task: JoinHandle<()>,
}

impl Subscription for KubeSubscription {
    fn cancel(&mut self) {
        self.task.abort();
    }
}

impl Drop for KubeSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct NoopSubscription;

impl Subscription for NoopSubscription {
    fn cancel(&mut self) {}
}

/// Reports the session error when no cluster is available.
struct UnavailableSource {
    reason: String,
}

impl UnavailableSource {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl ResourceSource for UnavailableSource {
    fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
        let _ = events.send(SourceEvent::Error {
            reason: self.reason.clone(),
        });
        Box::new(NoopSubscription)
    }

    fn subscribe_bounded(&mut self, events: mpsc::Sender<SourceEvent>) -> Box<dyn Subscription> {
        let _ = events.try_send(SourceEvent::Error {
            reason: self.reason.clone(),
        });
        Box::new(NoopSubscription)
    }
}

/// Loads and refreshes the Kubernetes resource catalog.
#[derive(Clone)]
pub struct CatalogHandle {
    handle: Handle,
    registry: Arc<ClusterRegistry>,
    cluster: ClusterId,
}

impl CatalogHandle {
    pub fn spawn_load(&self) -> JoinHandle<Result<ResourceCatalog, String>> {
        self.spawn_with(false)
    }

    /// Reloads discovery data without using the cluster cache.
    pub fn spawn_refresh(&self) -> JoinHandle<Result<ResourceCatalog, String>> {
        self.spawn_with(true)
    }

    /// Loads a validated catalog from the disk cache.
    pub fn spawn_cached(
        &self,
        cache: Arc<ClusterCache>,
    ) -> JoinHandle<Option<k8s_core::cache::CachedCatalog>> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        self.handle.spawn(async move {
            let store = cache.discovery(&registry).await?;
            let version = server_version(&registry, cluster).await?;
            tokio::task::spawn_blocking(move || store.load(&version))
                .await
                .ok()
                .flatten()
        })
    }

    /// Saves a catalog to the disk cache.
    pub fn spawn_cache_save(&self, cache: Arc<ClusterCache>, catalog: ResourceCatalog) {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        self.handle.spawn(async move {
            let Some(store) = cache.discovery(&registry).await else {
                return;
            };
            let version = server_version(&registry, cluster).await;
            let Some(version) = version else {
                return;
            };
            enqueue_discovery_write(&Handle::current(), store, catalog, version).await;
        });
    }

    fn spawn_with(&self, fresh: bool) -> JoinHandle<Result<ResourceCatalog, String>> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        self.handle.spawn(async move {
            let Some(cluster) = registry.get(cluster) else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            let catalog = if fresh {
                ResourceCatalog::fetch_fresh(cluster).await
            } else {
                ResourceCatalog::fetch(cluster).await
            };
            catalog.map_err(|error| {
                format!(
                    "Kubernetes resource types did not load: {error}. Check cluster access and try again."
                )
            })
        })
    }
}

/// Returns the API server version used to validate the discovery cache.
async fn server_version(registry: &Arc<ClusterRegistry>, cluster: ClusterId) -> Option<String> {
    data_source_for_cluster(registry, cluster)?
        .port()
        .server_version()
        .await
        .ok()
}

impl ClusterSession {
    pub fn load_hotbar(registry: &ClusterRegistry) -> (Hotbar, Option<HotbarError>) {
        let index = cluster_index(registry);
        Hotbar::load_default(|key| index.get(key).copied())
    }

    #[cfg(test)]
    pub fn from_registry(registry: Arc<ClusterRegistry>, handle: Handle) -> Self {
        Self::from_registry_with_cluster(registry, handle, None)
    }

    #[cfg(not(test))]
    pub fn from_registry(registry: Arc<ClusterRegistry>, handle: Handle) -> Self {
        let (hotbar, error) = Self::load_hotbar(&registry);
        Self::from_loaded_hotbar(registry, handle, hotbar, error)
    }

    fn from_loaded_hotbar(
        registry: Arc<ClusterRegistry>,
        handle: Handle,
        hotbar: Hotbar,
        _error: Option<HotbarError>,
    ) -> Self {
        let selected = hotbar
            .active_bank()
            .and_then(|bank| bank.slots.first())
            .map(|slot| slot.cluster_id);
        Self::from_registry_with_cluster(registry, handle, selected)
    }

    pub fn from_registry_with_cluster(
        registry: Arc<ClusterRegistry>,
        handle: Handle,
        selected: Option<ClusterId>,
    ) -> Self {
        let cluster = match selected {
            Some(cluster) if registry.get(cluster).is_some() => cluster,
            Some(_) => {
                return Self::unavailable_with_registry(
                    registry,
                    handle,
                    SELECTED_CONTEXT_UNAVAILABLE.to_owned(),
                );
            }
            None => match registry.clusters().first().map(Cluster::id) {
                Some(cluster) => cluster,
                None => {
                    return Self::unavailable_with_registry(
                        registry,
                        handle,
                        SELECTED_CONTEXT_UNAVAILABLE.to_owned(),
                    );
                }
            },
        };
        let cache = ClusterCache::from_settings_with_service(
            &registry,
            cluster,
            data_source_for_cluster(&registry, cluster),
        );
        if let Some(cache) = &cache {
            cache.spawn_uid_warm(Arc::clone(&registry), &handle);
        }
        Self::Ready {
            registry,
            cluster,
            handle,
            cache,
        }
    }

    pub fn reload_hotbar_session(&self) -> Result<Self, String> {
        let registry = Arc::clone(
            self.registry()
                .ok_or_else(|| SELECTED_CONTEXT_UNAVAILABLE.to_owned())?,
        );
        let handle = self
            .tokio_handle()
            .ok_or_else(|| SELECTED_CONTEXT_UNAVAILABLE.to_owned())?
            .clone();
        let (hotbar, _error) = Self::load_hotbar(&registry);
        let selected = hotbar
            .active_bank()
            .and_then(|bank| bank.slots.first())
            .map(|slot| slot.cluster_id);
        Ok(Self::from_registry_with_cluster(registry, handle, selected))
    }

    fn unavailable_with_registry(
        registry: Arc<ClusterRegistry>,
        handle: Handle,
        reason: String,
    ) -> Self {
        Self::Unavailable {
            reason,
            registry: Some(registry),
            handle: Some(handle),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            registry: None,
            handle: None,
        }
    }

    /// Returns the loaded cluster registry.
    pub fn registry(&self) -> Option<&Arc<ClusterRegistry>> {
        match self {
            Self::Ready { registry, .. } => Some(registry),
            Self::Unavailable { registry, .. } => registry.as_ref(),
        }
    }

    pub fn cluster_id(&self) -> Option<ClusterId> {
        match self {
            Self::Ready { cluster, .. } => Some(*cluster),
            Self::Unavailable { .. } => None,
        }
    }

    /// Returns the runtime bound to the cluster session.
    pub fn tokio_handle(&self) -> Option<&Handle> {
        match self {
            Self::Ready { handle, .. } => Some(handle),
            Self::Unavailable { handle, .. } => handle.as_ref(),
        }
    }

    /// Returns the current context name.
    pub fn cluster_name(&self) -> Option<&str> {
        match self {
            Self::Ready {
                registry, cluster, ..
            } => registry.get(*cluster).map(Cluster::name),
            Self::Unavailable { .. } => None,
        }
    }

    /// Switches to a context by name.
    /// Failed contexts keep the registry so the user can switch back.
    pub fn switch_to_context(&self, name: &str) -> Option<Self> {
        let registry = Arc::clone(self.registry()?);
        let handle = self.tokio_handle()?.clone();
        if let Some(cluster) = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == name)
        {
            let cluster = cluster.id();
            let cache = ClusterCache::from_settings_with_service(
                &registry,
                cluster,
                data_source_for_cluster(&registry, cluster),
            );
            if let Some(cache) = &cache {
                cache.spawn_uid_warm(Arc::clone(&registry), &handle);
            }
            return Some(Self::Ready {
                registry,
                cluster,
                handle,
                cache,
            });
        }
        if !registry
            .context_errors()
            .iter()
            .any(|error| error.context == name)
        {
            return None;
        }
        Some(Self::Unavailable {
            reason: format!(
                "The selected context {name} did not load. Reload Kubeconfigs and try again."
            ),
            registry: Some(registry),
            handle: Some(handle),
        })
    }

    /// Returns the current context health snapshot.
    pub fn health(&self) -> Option<Health> {
        let registry = self.registry()?;
        let cluster = registry.get(self.cluster_id()?)?;
        Some(cluster.health_snapshot())
    }

    /// Returns an Inspector source tied to the current session.
    pub fn inspector_source(&self) -> Arc<dyn InspectorSource> {
        match self.cluster_handle() {
            Some(handle) => Arc::new(handle),
            None => Arc::new(OfflineInspectorSource {
                reason: match self {
                    Self::Unavailable { reason, .. } => reason.clone(),
                    Self::Ready { .. } => SELECTED_CONTEXT_UNAVAILABLE.to_owned(),
                },
            }),
        }
    }

    /// Returns a log factory for the current cluster.
    pub fn log_factory(&self) -> Option<LogFactory> {
        self.cluster_handle().map(|handle| handle.log_factory())
    }

    /// Returns a resource catalog loader for the current cluster.
    pub fn catalog(&self) -> Option<CatalogHandle> {
        match self {
            Self::Ready {
                registry,
                cluster,
                handle,
                ..
            } => Some(CatalogHandle {
                handle: handle.clone(),
                registry: Arc::clone(registry),
                cluster: *cluster,
            }),
            Self::Unavailable { .. } => None,
        }
    }

    /// Narrows namespaced resources to one namespace.
    pub fn scope_for(entry: &ResourceEntry, namespace: Option<&str>) -> Scope {
        match namespace {
            Some(namespace) if !namespace.is_empty() && entry.namespaced() => {
                Scope::Namespace(namespace.to_owned())
            }
            _ => Scope::All,
        }
    }

    /// Returns the disk cache for the current cluster.
    pub fn cluster_cache(&self) -> Option<Arc<ClusterCache>> {
        match self {
            Self::Ready { cache, .. } => cache.clone(),
            Self::Unavailable { .. } => None,
        }
    }

    /// Creates a source factory for a resource entry and namespace.
    pub fn source_factory(&self, entry: &ResourceEntry, namespace: Option<&str>) -> SourceFactory {
        match self {
            Self::Ready {
                registry,
                cluster,
                handle,
                cache,
            } => {
                let registry = Arc::clone(registry);
                let cluster = *cluster;
                let handle = handle.clone();
                let scope = Self::scope_for(entry, namespace);
                let resource = entry.to_api_resource();
                let cache = cache.clone();
                Box::new(move || {
                    Box::new(
                        KubeSource::new(
                            handle.clone(),
                            Arc::clone(&registry),
                            cluster,
                            resource.clone(),
                            scope.clone(),
                            WatchOptions::default(),
                        )
                        .with_cache(cache.clone()),
                    )
                })
            }
            Self::Unavailable { reason, .. } => {
                let reason = reason.clone();
                Box::new(move || Box::new(UnavailableSource::new(reason.clone())))
            }
        }
    }

    /// Creates a Pod source factory.
    pub fn pods_factory(&self, namespace: Option<&str>) -> SourceFactory {
        self.source_factory(&pods_entry(), namespace)
    }

    /// Returns a UI cluster handle for the current session.
    pub fn cluster_handle(&self) -> Option<ClusterHandle> {
        match self {
            Self::Ready {
                registry,
                cluster,
                handle,
                ..
            } => Some(ClusterHandle {
                handle: handle.clone(),
                registry: Arc::clone(registry),
                cluster: *cluster,
            }),
            Self::Unavailable { .. } => None,
        }
    }
}

async fn send_log_event(sink: &LogSink, event: LogEvent) -> bool {
    sink.send(event.bounded()).await.is_ok()
}

/// Runs Kubernetes operations for the UI on the session runtime.
#[derive(Clone)]
pub struct ClusterHandle {
    handle: Handle,
    registry: Arc<ClusterRegistry>,
    cluster: ClusterId,
}

impl ClusterHandle {
    /// Creates a log stream factory.
    pub fn log_factory(&self) -> LogFactory {
        let this = self.clone();
        std::rc::Rc::new(move |request, options, sink| {
            this.spawn_log_stream(request, options, sink)
        })
    }

    fn spawn_log_stream(
        &self,
        request: LogRequest,
        options: ops::LogOptions,
        sink: LogSink,
    ) -> Box<dyn LogSubscription> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let task = self.handle.spawn(async move {
            let Some(cluster) = registry.get(cluster) else {
                let _ = send_log_event(
                    &sink,
                    LogEvent::Ended(
                        SELECTED_CONTEXT_UNAVAILABLE
                            .to_owned(),
                    ),
                )
                .await;
                return;
            };
            let client = cluster.client().clone();
            let resource = pods_resource();
            let namespace = request.namespace.as_deref();
            match ops::log_stream(&client, &resource, namespace, &request.name, &options).await {
                Ok(mut stream) => loop {
                    match stream.next().await {
                        Some(ops::LogItem::Line(line)) => {
                            if !send_log_event(&sink, LogEvent::Line(line)).await {
                                return;
                            }
                        }
                        Some(ops::LogItem::Error(error)) => {
                            let _ = send_log_event(
                                &sink,
                                LogEvent::Ended(format!(
                                    "The log stream failed: {error}. Refresh the log view to reconnect."
                                )),
                            )
                            .await;
                            return;
                        }
                        None => {
                            let _ = send_log_event(
                                &sink,
                                LogEvent::Ended("The log stream ended.".to_owned()),
                            )
                            .await;
                            return;
                        }
                    }
                },
                Err(ops::OpsError::ContainerSelectionRequired { containers, .. }) => {
                    let _ = send_log_event(
                        &sink,
                        LogEvent::Ended(format!(
                            "Select a container to stream: {}",
                            containers.join(", ")
                        )),
                    )
                    .await;
                }
                Err(error) => {
                    let _ = send_log_event(
                        &sink,
                        LogEvent::Ended(format!(
                            "The log request failed: {error}. Check the Pod, cluster connection, and access permissions, then try again."
                        )),
                    )
                    .await;
                }
            }
        });
        Box::new(KubeLogSubscription { task })
    }

    fn describe_future(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let object = object.clone();
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::describe(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.name,
                )
                .await
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Describe request failed: {error}. Check the cluster connection and access permissions, then try again."
                )
            })?
        })
    }

    fn events_future(&self, object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let object = object.clone();
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::list_events_for(&client, object.namespace.as_deref(), &object.uid)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Events request failed: {error}. Check the cluster connection and access permissions, then try again."
                )
            })?
        })
    }

    /// Reads one named object and reports whether it is there.
    ///
    /// One `GET`, and the answer is three-valued because the caller's next step
    /// differs three ways. `Found` carries the uid because a name alone is not an
    /// object — the name can be freed and taken by a different one, and a Related
    /// row that opened the replacement would be a claim about an object the panel
    /// never described. `Missing` means the API server answered, so it is the only
    /// answer that may paint a reference as broken. Everything else — a refused
    /// verb, a dead connection, a 30s timeout — is `Unknown`, because an offline
    /// session that answered `Missing` would paint every reference in the panel
    /// `danger` and teach the reader that the panel is lying.
    ///
    /// The error variants are mapped rather than string-matched. `ops` already
    /// decided which HTTP status is "no such object" (404 on a named read) and
    /// which is a broken connection, and re-deciding that here from prose would
    /// make the two disagree the first time the apiserver reworded an error.
    fn resolve_future(&self, object: &ObjectRef) -> OpsFuture<ResolveOutcome> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let object = object.clone();
        let entry = entry_for(&object);
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Ok(ResolveOutcome::Unknown);
            };
            // A context that went away is not an answer about the object, so it
            // answers `Unknown` rather than failing the call: a caller that cannot
            // reach a cluster has not learned that anything is missing.
            let data = join_abortable(&handled, async move {
                ops::read_resource(
                    &client,
                    &entry,
                    ops::ResourceReadOptions {
                        namespace: object.namespace.as_deref(),
                        name: Some(&object.name),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .map_err(|error| format!("The resolve request failed: {error}. Try again."))?;
            Ok(match data {
                Ok(ops::ResourceData::One(object)) => {
                    // An object with no uid is not something this can vouch for, and
                    // reporting `Found` without one would hand the caller a claim it
                    // cannot check.
                    match object.metadata.uid.as_deref() {
                        Some(uid) if !uid.is_empty() => ResolveOutcome::Found {
                            uid: uid.to_owned(),
                        },
                        _ => ResolveOutcome::Unknown,
                    }
                }
                Ok(ops::ResourceData::List(_)) => ResolveOutcome::Unknown,
                Err(error) => resolve_outcome_for(&error),
            })
        })
    }

    pub fn read_service_account(&self, target: &ServiceAccountTarget) -> OpsFuture<DynamicObject> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let namespace = target.namespace.clone();
        let name = target.name.clone();
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                match ops::read_resource(
                    &client,
                    &service_accounts_entry(),
                    ops::ResourceReadOptions {
                        namespace: Some(&namespace),
                        name: Some(&name),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error: ops::ResourceReadError| error.to_string())?
                {
                    ops::ResourceData::One(object) => Ok(*object),
                    ops::ResourceData::List(_) => Err(
                        "The Service Account read returned a list instead of one object."
                            .to_owned(),
                    ),
                }
            })
            .await
            .map_err(|error| format!("The Service Account read task failed: {error}"))?
        })
    }

    /// Asks the API server to validate a document without storing it.
    ///
    /// A local parse cannot see a schema violation, an immutable field, or a missing required
    /// field, and those are the failures that surface after an apply has already half-succeeded.
    /// This runs the same server-side apply the real request would run, in dry-run mode, and
    /// hands back the server's verdict so the review can state it before anything is written.
    pub fn check_apply_future(
        &self,
        object: &ObjectRef,
        yaml: String,
    ) -> OpsFuture<ops::ApplyCheck> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let object = object.clone();
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::check_apply_yaml(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.uid,
                    &yaml,
                )
                .await
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| {
                format!("The check request failed: {error}. Try again, or apply without checking.")
            })?
        })
    }

    /// Applies YAML and returns the server result.
    pub fn apply_future(&self, object: &ObjectRef, yaml: String) -> OpsFuture<ApplyOutcome> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let object = object.clone();
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::apply_yaml(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.uid,
                    &yaml,
                )
                .await
                .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Apply request failed: {error}. Fix the YAML or update access permissions, then try again."
                )
            })?
        })
    }
}

impl InspectorSource for ClusterHandle {
    fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
        self.describe_future(object)
    }

    fn events(&self, object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
        self.events_future(object)
    }

    fn resolve(&self, object: &ObjectRef) -> OpsFuture<ResolveOutcome> {
        self.resolve_future(object)
    }
}

/// Runs table operations against the current cluster.
impl ObjectOps for ClusterHandle {
    fn delete(&self, object: ObjectRef) -> OpsFuture<()> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::delete_object(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.name,
                    &object.uid,
                )
                .await
                .map(|_| ())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Delete request failed: {error}. Check the resource and access permissions, then try again."
                )
            })?
            .map_err(|error| error.to_string())
        })
    }

    fn scale(&self, object: ObjectRef, replicas: i32) -> OpsFuture<()> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::scale(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.name,
                    &object.uid,
                    replicas,
                )
                .await
                .map(|_| ())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Scale request failed: {error}. Check the replica count and access permissions, then try again."
                )
            })?
            .map_err(|error| error.to_string())
        })
    }

    fn restart(&self, object: ObjectRef) -> OpsFuture<()> {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let handled = self.handle.clone();
        Box::pin(async move {
            let Some(client) = registry
                .get(cluster)
                .map(|cluster| cluster.client().clone())
            else {
                return Err(SELECTED_CONTEXT_UNAVAILABLE.to_owned());
            };
            join_abortable(&handled, async move {
                ops::rollout_restart(
                    &client,
                    &object.resource,
                    object.namespace.as_deref(),
                    &object.name,
                    &object.uid,
                )
                .await
                .map(|_| ())
            })
            .await
            .map_err(|error| {
                format!(
                    "The Restart request failed: {error}. Check the resource and access permissions, then try again."
                )
            })?
            .map_err(|error| error.to_string())
        })
    }
}

/// Reports the session error for Inspector requests.
struct OfflineInspectorSource {
    reason: String,
}

impl InspectorSource for OfflineInspectorSource {
    fn describe(&self, _object: &ObjectRef) -> OpsFuture<DescribeData> {
        let reason = self.reason.clone();
        Box::pin(async move { Err(reason) })
    }

    fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
        let reason = self.reason.clone();
        Box::pin(async move { Err(reason) })
    }

    // `resolve` is deliberately not overridden. The trait's default already says
    // `Unknown`, which is the truth for a source with no connection: it cannot know
    // that anything is missing. Overriding it to fail like `describe` and `events`
    // would be a different truth — one that a caller would have to learn to
    // distinguish from a permission refusal.
}

struct KubeLogSubscription {
    task: JoinHandle<()>,
}

impl LogSubscription for KubeLogSubscription {
    fn cancel(&mut self) {
        self.task.abort();
    }
}

impl Drop for KubeLogSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube_core::DynamicObject;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static SESSION_CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn pod(name: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": format!("uid-{name}"),
                },
            }))
            .expect("valid Pod object"),
        )
    }

    #[test]
    fn tier_maps_to_watch_options() {
        let base = WatchOptions {
            label_selector: Some("app=web".to_owned()),
            field_selector: Some("metadata.name=web".to_owned()),
            tier: LatencyTier::Local,
        };
        let high = options_for_tier(&base, LatencyTier::HighLatency);
        assert_eq!(high.tier, LatencyTier::HighLatency);
        assert_eq!(high.label_selector, base.label_selector);
        assert_eq!(high.field_selector, base.field_selector);
        let local = options_for_tier(&high, LatencyTier::Local);
        assert_eq!(local.tier, LatencyTier::Local);
    }

    #[test]
    fn maps_all_controller_events() {
        assert!(matches!(
            map_controller_event(ControllerEvent::Init),
            SourceEvent::Init
        ));

        match map_controller_event(ControllerEvent::InitApply(pod("alpha"))) {
            SourceEvent::Store(StoreEvent { op, obj }) => {
                assert_eq!(op, StoreOp::Apply, "InitApply has Apply semantics");
                assert_eq!(obj.metadata.name.as_deref(), Some("alpha"));
            }
            other => panic!("InitApply must map to Store, got {other:?}"),
        }

        assert!(matches!(
            map_controller_event(ControllerEvent::InitDone),
            SourceEvent::InitDone
        ));

        match map_controller_event(ControllerEvent::Store(StoreEvent {
            op: StoreOp::Delete,
            obj: pod("alpha"),
        })) {
            SourceEvent::Store(StoreEvent { op, .. }) => assert_eq!(op, StoreOp::Delete),
            other => panic!("Store must forward unchanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bounded_source_sender_backpressures_a_slow_consumer() {
        let (sender, mut receiver) = mpsc::channel(1);
        let events = EventSender::Bounded(sender);
        assert!(events.send(SourceEvent::Init).await);
        let pending = events.send(SourceEvent::InitDone);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        assert!(matches!(receiver.recv().await, Some(SourceEvent::Init)));
        assert!(pending.await);
        assert!(matches!(receiver.recv().await, Some(SourceEvent::InitDone)));
    }

    #[tokio::test]
    async fn bounded_log_sink_backpressures_and_limits_slow_batches() {
        let (sink, mut receiver) = mpsc::channel(1);
        assert!(send_log_event(&sink, LogEvent::Line("first".to_owned())).await);
        let pending = send_log_event(
            &sink,
            LogEvent::Line("你".repeat(crate::session::LOG_EVENT_MAX_BYTES)),
        );
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        assert!(matches!(
            receiver.recv().await,
            Some(LogEvent::Line(line)) if line == "first"
        ));
        assert!(pending.await);
        let Some(LogEvent::Line(line)) = receiver.recv().await else {
            panic!("bounded line must arrive");
        };
        assert!(line.len() <= crate::session::LOG_EVENT_MAX_BYTES);
    }

    #[tokio::test]
    async fn dropping_log_subscription_aborts_a_blocked_sender() {
        let (sink, receiver) = mpsc::channel(1);
        assert!(send_log_event(&sink, LogEvent::Line("first".to_owned())).await);
        let task = tokio::spawn(async move {
            let _ = send_log_event(&sink, LogEvent::Line("blocked".to_owned())).await;
        });
        let abort = task.abort_handle();
        tokio::task::yield_now().await;
        assert!(!abort.is_finished());

        drop(KubeLogSubscription { task });
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        drop(receiver);
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn canceled_operation_future_aborts_inner_task() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let handle = Handle::current();
        let mut future = Box::pin(join_abortable(&handle, async move {
            let _guard = DropFlag(flag);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        }));

        tokio::select! {
            result = &mut future => panic!("operation future must remain pending: {result:?}"),
            _ = started_rx => {}
        }
        drop(future);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("inner operation task must be aborted");
    }

    #[tokio::test]
    async fn forwards_controller_events_in_order() {
        let (sender, receiver) = mpsc::channel(16);
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(5),
            None,
        ));

        for event in [
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("alpha")),
            ControllerEvent::InitDone,
            ControllerEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj: pod("alpha"),
            }),
        ] {
            sender.send(event).await.expect("forwarding task is alive");
        }
        drop(sender);
        forward.await.expect("forwarding task ends normally");

        let mut received = Vec::new();
        while let Ok(event) = forwarded.try_recv() {
            received.push(event);
        }
        assert_eq!(received.len(), 5, "four events plus the closed-input error");
        assert!(matches!(received[0], SourceEvent::Init));
        assert!(matches!(
            received[1],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                ..
            })
        ));
        assert!(matches!(received[2], SourceEvent::InitDone));
        assert!(matches!(
            received[3],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                ..
            })
        ));
        assert!(matches!(received[4], SourceEvent::Error { .. }));
    }

    #[tokio::test]
    async fn forwarding_accepts_a_bounded_source_sender() {
        let (sender, receiver) = mpsc::channel(1);
        let (events, mut forwarded) = mpsc::channel(8);
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(5),
            None,
        ));
        sender
            .send(ControllerEvent::Init)
            .await
            .expect("input is alive");
        sender
            .send(ControllerEvent::InitApply(pod("alpha")))
            .await
            .expect("input is alive");
        sender
            .send(ControllerEvent::InitDone)
            .await
            .expect("input is alive");
        drop(sender);
        forward.await.expect("forwarding task ends");
        assert!(matches!(forwarded.recv().await, Some(SourceEvent::Init)));
        assert!(matches!(
            forwarded.recv().await,
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                ..
            }))
        ));
        assert!(matches!(
            forwarded.recv().await,
            Some(SourceEvent::InitDone)
        ));
    }

    #[tokio::test]
    async fn bounded_forwarding_preserves_boundaries_and_final_store_event() {
        let (sender, receiver) = mpsc::channel(16);
        let (events, mut forwarded) = mpsc::channel(16);
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(5),
            None,
        ));

        for event in [
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("alpha")),
            ControllerEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj: pod("alpha"),
            }),
            ControllerEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj: pod("alpha"),
            }),
            ControllerEvent::InitDone,
        ] {
            sender.send(event).await.expect("input is alive");
        }
        drop(sender);
        forward.await.expect("forwarding task ends");

        let mut received = Vec::new();
        while let Ok(event) = forwarded.try_recv() {
            received.push(event);
        }
        assert_eq!(received.len(), 6);
        assert!(matches!(&received[0], SourceEvent::Init));
        assert!(matches!(
            &received[1],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-alpha")
        ));
        assert!(matches!(
            &received[2],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-alpha")
        ));
        assert!(matches!(
            &received[3],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-alpha")
        ));
        assert!(matches!(&received[4], SourceEvent::InitDone));
        assert!(matches!(&received[5], SourceEvent::Error { .. }));
    }

    #[tokio::test]
    async fn initial_sync_timeout_reports_error() {
        let (_sender, receiver) = mpsc::channel(1);
        let (events, mut forwarded) = mpsc::unbounded_channel();

        forward_events(receiver, events, Duration::from_millis(20), None).await;

        match forwarded.recv().await {
            Some(SourceEvent::Error { reason }) => {
                assert!(
                    reason.contains("Initial data load"),
                    "reason must be readable: {reason}"
                );
            }
            other => panic!("initial load timeout must report Error, got {other:?}"),
        }
    }

    /// A list the cluster refuses outright must reach the error state, not the
    /// loading one.
    ///
    /// `Controller` re-arms its init boundary on every failed list and rebuilds, so
    /// a refused identity sends `Init` again and again. The 30-second budget used to
    /// be re-armed with each one, which made the timeout unreachable for exactly the
    /// failures it exists to report: an RBAC refusal and a resource type the cluster
    /// does not serve both retry forever, and the reader was left on a skeleton with
    /// `Loading Pods…` for as long as they cared to look. Measured on a
    /// namespaces-only identity against a Pods table: still loading after 45s.
    ///
    /// The budget is the whole point of the test: with a real `Duration` this would
    /// take 30 seconds, so the test drives the same supervisor shape with 20ms and
    /// asserts the *outcome* — an `Error` — rather than the clock.
    #[tokio::test]
    async fn a_refused_list_reaches_the_error_state_instead_of_loading_forever() {
        let (sender, receiver) = mpsc::channel(8);
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_millis(20),
            None,
        ));

        // The retry storm: an `Init`, a failed attempt, another `Init`, and so on,
        // faster than the budget. Every one of these re-armed the deadline before
        // the fix, so the budget never expired and the loop never ended.
        for _ in 0..12 {
            let _ = sender.send(ControllerEvent::Init).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let mut errors = 0;
        while let Ok(event) = forwarded.try_recv() {
            match event {
                SourceEvent::Error { reason } => {
                    assert!(
                        reason.contains("Initial data load"),
                        "the reader is told the load did not finish, in words: {reason}"
                    );
                    errors += 1;
                }
                SourceEvent::Init | SourceEvent::LatencyUpdated { .. } => {}
                other => panic!("a refused list must not deliver data: {other:?}"),
            }
        }
        assert_eq!(
            errors, 1,
            "the budget expires once and reports once — an unbounded retry is not a \
             reason to keep resetting it"
        );
        assert!(
            forward.is_finished(),
            "the supervisor stops on the failure instead of retrying silently"
        );
    }

    #[tokio::test]
    async fn timeout_is_disarmed_after_init_done() {
        let (sender, receiver) = mpsc::channel(16);
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_millis(100),
            None,
        ));

        sender
            .send(ControllerEvent::Init)
            .await
            .expect("forwarding task is alive");
        sender
            .send(ControllerEvent::InitDone)
            .await
            .expect("forwarding task is alive");
        assert!(matches!(forwarded.recv().await, Some(SourceEvent::Init)));
        assert!(matches!(
            forwarded.recv().await,
            Some(SourceEvent::InitDone)
        ));

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            forwarded.try_recv().is_err(),
            "InitDone disables the initial load timeout"
        );
        forward.abort();
    }

    #[tokio::test]
    async fn post_init_reconnect_rearms_initial_timeout() {
        let (sender, receiver) = mpsc::channel(16);
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_millis(200),
            None,
        ));

        sender
            .send(ControllerEvent::Init)
            .await
            .expect("forwarding task is alive");
        sender
            .send(ControllerEvent::InitDone)
            .await
            .expect("forwarding task is alive");
        assert!(matches!(forwarded.recv().await, Some(SourceEvent::Init)));
        assert!(matches!(
            forwarded.recv().await,
            Some(SourceEvent::InitDone)
        ));
        tokio::time::sleep(Duration::from_millis(120)).await;
        sender
            .send(ControllerEvent::Init)
            .await
            .expect("forwarding task is alive");
        assert!(matches!(forwarded.recv().await, Some(SourceEvent::Init)));
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(forwarded.try_recv().is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(
            forwarded.recv().await,
            Some(SourceEvent::Error { .. })
        ));
        forward.await.expect("forwarding task ends");
    }

    #[tokio::test]
    async fn new_init_clears_partial_objects_before_authoritative_list() {
        let sequence = SESSION_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "k8s-gpui-init-boundary-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = SnapshotCache::new(
            &root,
            ClusterId::derive("ctx", "https://init-boundary.example.com:6443"),
            "uid-init-boundary",
            true,
        );
        let gvr = GvrKey::new("", "v1", "pods");
        let mut saver = Some(SnapshotSaver::new(
            store.clone(),
            gvr.clone(),
            next_cache_write_generation(),
        ));
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let events = EventSender::Unbounded(events);

        for event in [
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("old")),
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("new")),
        ] {
            assert!(forward_controller_event(event, &events, &mut saver).await);
        }

        let saved_objects = saver
            .as_ref()
            .expect("saver remains pending after the initial list fails")
            .objects
            .values()
            .filter_map(|object| object.metadata.name.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(saved_objects, ["new"]);
        assert!(forward_controller_event(ControllerEvent::InitDone, &events, &mut saver).await);

        let mut received = Vec::new();
        while let Ok(event) = forwarded.try_recv() {
            received.push(event);
        }
        assert_eq!(received.len(), 5);
        assert!(matches!(&received[0], SourceEvent::Init));
        assert!(matches!(
            &received[1],
            SourceEvent::Store(StoreEvent { obj, .. })
                if obj.metadata.name.as_deref() == Some("old")
        ));
        assert!(matches!(&received[2], SourceEvent::Init));
        assert!(matches!(
            &received[3],
            SourceEvent::Store(StoreEvent { obj, .. })
                if obj.metadata.name.as_deref() == Some("new")
        ));
        assert!(matches!(&received[4], SourceEvent::InitDone));

        let loaded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = store.load(&gvr) {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("disk write must finish before timeout");
        let names: Vec<_> = loaded
            .objects
            .iter()
            .filter_map(|object| object.metadata.name.as_deref())
            .collect();
        assert_eq!(names, ["new"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn snapshot_saver_saves_again_after_init_done_and_debounces_apply() {
        let sequence = SESSION_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "k8s-gpui-snapshot-debounce-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = SnapshotCache::new(
            &root,
            ClusterId::derive("ctx", "https://snapshot-debounce.example.com:6443"),
            "uid-snapshot-debounce",
            true,
        );
        let gvr = GvrKey::new("", "v1", "pods");
        let mut saver = Some(SnapshotSaver::new(
            store.clone(),
            gvr.clone(),
            next_cache_write_generation(),
        ));
        let (sender, _forwarded) = mpsc::unbounded_channel();
        let events = EventSender::Unbounded(sender);

        for event in [
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("alpha")),
            ControllerEvent::InitDone,
        ] {
            assert!(forward_controller_event(event, &events, &mut saver).await);
        }
        assert!(saver.is_some(), "InitDone must retain the saver");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = store.load(&gvr) {
                    let mut names: Vec<_> = snapshot
                        .objects
                        .iter()
                        .filter_map(|object| object.metadata.name.as_deref())
                        .map(str::to_owned)
                        .collect();
                    names.sort();
                    if names == ["alpha".to_owned()] {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial snapshot must be saved");

        assert!(
            forward_controller_event(
                ControllerEvent::Store(StoreEvent {
                    op: StoreOp::Apply,
                    obj: pod("beta"),
                }),
                &events,
                &mut saver,
            )
            .await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before_debounce = store.load(&gvr).expect("initial snapshot must remain");
        assert_eq!(
            before_debounce
                .objects
                .iter()
                .filter_map(|object| object.metadata.name.as_deref())
                .collect::<Vec<_>>(),
            ["alpha"]
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = store.load(&gvr) {
                    let mut names: Vec<_> = snapshot
                        .objects
                        .iter()
                        .filter_map(|object| object.metadata.name.as_deref())
                        .map(str::to_owned)
                        .collect();
                    names.sort();
                    if names == ["alpha".to_owned(), "beta".to_owned()] {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("incremental snapshot must be saved after debounce");
        assert!(saver.is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn snapshot_saver_does_not_write_during_partial_reinit() {
        let sequence = SESSION_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "k8s-gpui-snapshot-reinit-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = SnapshotCache::new(
            &root,
            ClusterId::derive("ctx", "https://snapshot-reinit.example.com:6443"),
            "uid-snapshot-reinit",
            true,
        );
        let gvr = GvrKey::new("", "v1", "pods");
        let mut saver = Some(SnapshotSaver::new(
            store.clone(),
            gvr.clone(),
            next_cache_write_generation(),
        ));
        let (sender, _forwarded) = mpsc::unbounded_channel();
        let events = EventSender::Unbounded(sender);

        for event in [
            ControllerEvent::Init,
            ControllerEvent::InitApply(pod("alpha")),
            ControllerEvent::InitDone,
        ] {
            assert!(forward_controller_event(event, &events, &mut saver).await);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.load(&gvr).is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial snapshot must be saved");
        std::fs::remove_file(store.path(&gvr)).expect("remove initial snapshot");

        assert!(forward_controller_event(ControllerEvent::Init, &events, &mut saver).await);
        assert!(
            forward_controller_event(
                ControllerEvent::Store(StoreEvent {
                    op: StoreOp::Apply,
                    obj: pod("beta"),
                }),
                &events,
                &mut saver,
            )
            .await
        );
        tokio::time::sleep(SNAPSHOT_SAVE_DEBOUNCE + Duration::from_millis(100)).await;
        assert!(!store.path(&gvr).exists());

        assert!(forward_controller_event(ControllerEvent::InitDone, &events, &mut saver).await);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = store.load(&gvr) {
                    let names: Vec<_> = snapshot
                        .objects
                        .iter()
                        .filter_map(|object| object.metadata.name.as_deref())
                        .collect();
                    if names == ["beta"] {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed reinit must be saved");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn closed_subscriber_stops_forwarding() {
        let (sender, receiver) = mpsc::channel(16);
        let (events, forwarded) = mpsc::unbounded_channel();
        drop(forwarded);
        let forward = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(5),
            None,
        ));

        sender
            .send(ControllerEvent::Init)
            .await
            .expect("forwarding task is alive");
        tokio::time::timeout(Duration::from_secs(1), forward)
            .await
            .expect("receiver drop must stop the task quickly")
            .expect("task ends normally");
    }

    #[tokio::test]
    async fn cancel_aborts_the_forwarding_task() {
        let (_sender, receiver) = mpsc::channel::<ControllerEvent>(1);
        let (events, _forwarded) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(60),
            None,
        ));
        let mut subscription = KubeSubscription { task };

        subscription.cancel();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            subscription.task.is_finished(),
            "cancel must abort the watch"
        );
    }

    #[tokio::test]
    async fn dropping_source_subscription_aborts_forwarding() {
        let (_sender, receiver) = mpsc::channel::<ControllerEvent>(1);
        let (events, _forwarded) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward_events(
            receiver,
            events,
            Duration::from_secs(60),
            None,
        ));
        let abort = task.abort_handle();
        drop(KubeSubscription { task });
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[tokio::test]
    async fn catalog_handle_reports_missing_cluster() {
        let registry = Arc::new(ClusterRegistry::default());
        let cluster = ClusterId::derive("gone", "https://gone.example.com:6443");
        let loader = CatalogHandle {
            handle: Handle::current(),
            registry,
            cluster,
        };

        let error = loader
            .spawn_load()
            .await
            .expect("task ends normally")
            .expect_err("unknown cluster must fail");
        assert_eq!(
            error,
            "The selected context is no longer available. Reload Kubeconfigs and try again."
        );
    }

    #[test]
    fn unavailable_session_has_no_catalog_and_keeps_erroring() {
        let session = ClusterSession::unavailable("no kubeconfig");
        assert!(
            session.catalog().is_none(),
            "no connection means no catalog loader"
        );

        let mut source = session.pods_factory(Some("kube-system"))();
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let _subscription = source.subscribe(events);
        match forwarded.try_recv() {
            Ok(SourceEvent::Error { reason }) => assert_eq!(reason, "no kubeconfig"),
            other => panic!("must fail immediately, got {other:?}"),
        }
    }

    #[test]
    fn unavailable_source_reports_error_on_subscribe() {
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let mut source = UnavailableSource::new("no kubeconfig");

        let _subscription = source.subscribe(events);

        match forwarded.try_recv() {
            Ok(SourceEvent::Error { reason }) => assert_eq!(reason, "no kubeconfig"),
            other => panic!("must fail immediately, got {other:?}"),
        }
        assert!(
            ClusterSession::unavailable("no kubeconfig")
                .cluster_name()
                .is_none(),
            "Unavailable sessions have no cluster name"
        );
    }

    #[test]
    fn scope_for_narrows_namespaced_resources_only() {
        let pods = pods_entry();
        assert_eq!(ClusterSession::scope_for(&pods, None), Scope::All);
        assert_eq!(ClusterSession::scope_for(&pods, Some("")), Scope::All);
        assert_eq!(
            ClusterSession::scope_for(&pods, Some("kube-system")),
            Scope::Namespace("kube-system".to_owned())
        );

        let mut nodes = pods_entry();
        nodes.kind = "Node".to_owned();
        nodes.plural = "nodes".to_owned();
        nodes.scope = ResourceScope::Cluster;
        assert_eq!(
            ClusterSession::scope_for(&nodes, Some("kube-system")),
            Scope::All,
            "cluster-scoped resources cannot use a namespace scope"
        );
    }

    #[tokio::test]
    async fn for_entry_builds_dynamic_source_for_any_gvr() {
        let registry = Arc::new(ClusterRegistry::default());
        let cluster = ClusterId::derive("kind", "https://kind.example.com:6443");
        let entry = ResourceEntry {
            group: "example.com".to_owned(),
            version: "v1".to_owned(),
            kind: "Widget".to_owned(),
            plural: "widgets".to_owned(),
            scope: ResourceScope::Namespaced,
            verbs: vec!["list".to_owned()],
        };

        let source = KubeSource::for_entry(
            Handle::current(),
            registry,
            cluster,
            &entry,
            Scope::Namespace("dev".to_owned()),
        );

        assert_eq!(source.resource.api_version, "example.com/v1");
        assert_eq!(source.resource.plural, "widgets");
        assert_eq!(source.resource.kind, "Widget");
        assert_eq!(source.scope, Scope::Namespace("dev".to_owned()));
    }

    #[test]
    fn entry_source_factory_keeps_unavailable_reason() {
        let session = ClusterSession::unavailable("no kubeconfig");
        let mut source = session.source_factory(&pods_entry(), Some("kube-system"))();
        let (events, mut forwarded) = mpsc::unbounded_channel();
        let _subscription = source.subscribe(events);
        match forwarded.try_recv() {
            Ok(SourceEvent::Error { reason }) => assert_eq!(reason, "no kubeconfig"),
            other => panic!("must fail immediately, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_with_unknown_cluster_reports_error() {
        let registry = Arc::new(ClusterRegistry::default());
        let cluster = ClusterId::derive("gone", "https://gone.example.com:6443");
        let mut source = KubeSource::pods(Handle::current(), registry, cluster);
        let (events, mut forwarded) = mpsc::unbounded_channel();

        let _subscription = source.subscribe(events);

        match forwarded.try_recv() {
            Ok(SourceEvent::Error { reason }) => assert_eq!(
                reason,
                "The selected context is no longer available. Reload Kubeconfigs and try again."
            ),
            other => panic!("unknown cluster must fail immediately, got {other:?}"),
        }
    }

    const SWITCH_KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
- name: beta
  cluster:
    server: http://127.0.0.1:6444
contexts:
- name: alpha-ctx
  context: { cluster: alpha, user: alpha-user }
- name: beta-ctx
  context: { cluster: beta, user: beta-user }
- name: broken-ctx
  context: { cluster: nowhere, user: alpha-user }
users:
- name: alpha-user
  user: {}
- name: beta-user
  user: {}
current-context: alpha-ctx
"#;

    async fn inline_switch_registry() -> Arc<ClusterRegistry> {
        switch_registry().await
    }

    async fn switch_registry() -> Arc<ClusterRegistry> {
        let sequence = SESSION_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "k8s-gpui-session-{}-{sequence}.yaml",
            std::process::id()
        ));
        std::fs::write(&path, SWITCH_KUBECONFIG).expect("write temporary kubeconfig");
        let registry = ClusterRegistry::load(&path)
            .await
            .expect("kubeconfig must load");
        let _ = std::fs::remove_file(&path);
        Arc::new(registry)
    }

    #[tokio::test]
    async fn startup_hotbar_selects_resolved_slot_before_first_context() {
        let registry = inline_switch_registry().await;
        let beta = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == "beta-ctx")
            .expect("beta context")
            .id();
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("bank");
        hotbar.add_slot(bank, beta, "beta-ctx").expect("slot");

        let session = ClusterSession::from_loaded_hotbar(
            Arc::clone(&registry),
            Handle::current(),
            hotbar,
            None,
        );
        assert_eq!(session.cluster_name(), Some("beta-ctx"));
    }

    #[tokio::test]
    async fn an_unresolved_startup_slot_never_blocks_the_current_context() {
        let registry = inline_switch_registry().await;
        // A hotbar slot is a convenience, not a claim about the app. One that
        // names a cluster the registry does not have used to take kubeconfig
        // loading down with it, leaving the window on "No Context" with an
        // error naming an internal cluster id.
        let session = ClusterSession::from_loaded_hotbar(
            Arc::clone(&registry),
            Handle::current(),
            Hotbar::default(),
            Some(HotbarError::UnresolvedSlot {
                cluster_id: "missing-id".to_owned(),
                label: "missing-ctx".to_owned(),
            }),
        );

        assert!(
            session.cluster_id().is_some(),
            "the app still loads its context"
        );
        assert_eq!(session.cluster_name(), Some("alpha-ctx"));
        assert!(
            session.switch_to_context("beta-ctx").is_some(),
            "and context switching still works"
        );
    }

    #[tokio::test]
    async fn empty_hotbar_keeps_first_context_default() {
        let registry = inline_switch_registry().await;
        let session = ClusterSession::from_loaded_hotbar(
            Arc::clone(&registry),
            Handle::current(),
            Hotbar::default(),
            None,
        );
        assert_eq!(session.cluster_name(), Some("alpha-ctx"));
    }

    #[tokio::test]
    async fn switch_to_context_round_trips_and_reports_broken_contexts() {
        let registry = switch_registry().await;
        assert_eq!(registry.clusters().len(), 2);
        assert_eq!(registry.context_errors().len(), 1);

        let session = ClusterSession::from_registry(Arc::clone(&registry), Handle::current());
        assert_eq!(session.cluster_name(), Some("alpha-ctx"));
        assert!(
            session.health().is_some(),
            "available contexts have health snapshots"
        );

        let beta = session
            .switch_to_context("beta-ctx")
            .expect("second context must be selectable");
        assert_eq!(beta.cluster_name(), Some("beta-ctx"));
        assert!(
            beta.switch_to_context("alpha-ctx").is_some(),
            "context switch must round-trip"
        );

        let broken = beta
            .switch_to_context("broken-ctx")
            .expect("failed context must remain selectable");
        assert!(broken.cluster_name().is_none());
        assert!(
            broken.catalog().is_none(),
            "failed contexts have no catalog loader"
        );
        assert!(
            broken.cluster_handle().is_none(),
            "failed contexts have no operation handle"
        );
        assert!(
            matches!(
                &broken,
                ClusterSession::Unavailable {
                    registry: Some(_),
                    handle: Some(_),
                    ..
                }
            ),
            "failed contexts retain the registry and handle for switching back"
        );
        let reason = match &broken {
            ClusterSession::Unavailable { reason, .. } => reason,
            _ => unreachable!(),
        };
        assert_eq!(
            reason,
            "The selected context broken-ctx did not load. Reload Kubeconfigs and try again."
        );

        let back = broken
            .switch_to_context("beta-ctx")
            .expect("a failed context must switch back to an available context");
        assert_eq!(back.cluster_name(), Some("beta-ctx"));

        assert!(session.switch_to_context("nope").is_none());
        assert!(
            ClusterSession::unavailable("no kubeconfig")
                .switch_to_context("alpha-ctx")
                .is_none(),
            "sessions without a registry cannot switch contexts"
        );
    }

    // Inspector requests must not fall back to an older cluster.
    #[tokio::test]
    async fn unavailable_session_inspector_source_reports_its_reason() {
        let session = ClusterSession::unavailable("no kubeconfig");
        let source = session.inspector_source();
        let object = ObjectRef {
            resource: pods_resource(),
            namespace: Some("default".to_owned()),
            name: "web-0".to_owned(),
            uid: "uid-web-0".to_owned(),
        };

        assert_eq!(
            source
                .describe(&object)
                .await
                .expect_err("request must fail"),
            "no kubeconfig"
        );
        assert_eq!(
            source.events(&object).await.expect_err("request must fail"),
            "no kubeconfig"
        );
    }

    // The cache layer removes Secret objects before saving.
    #[tokio::test]
    async fn cache_saver_persists_objects_and_drops_deleted() {
        let root = std::env::temp_dir().join(format!("k8s-gpui-saver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = SnapshotCache::new(
            &root,
            ClusterId::derive("ctx", "https://saver.example.com:6443"),
            "uid-1",
            true,
        );
        let gvr = GvrKey::new("", "v1", "pods");
        let mut saver =
            SnapshotSaver::new(store.clone(), gvr.clone(), next_cache_write_generation());
        saver.apply(&StoreEvent {
            op: StoreOp::Apply,
            obj: pod("alpha"),
        });
        saver.apply(&StoreEvent {
            op: StoreOp::Apply,
            obj: pod("beta"),
        });
        saver.apply(&StoreEvent {
            op: StoreOp::Delete,
            obj: pod("alpha"),
        });
        saver.save().await;

        let loaded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = store.load(&gvr) {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("disk write must finish before timeout");
        let names: Vec<&str> = loaded
            .objects
            .iter()
            .filter_map(|object| object.metadata.name.as_deref())
            .collect();
        assert_eq!(names, ["beta"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
