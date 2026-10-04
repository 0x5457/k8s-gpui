//! Cluster registry for kubeconfig loading, connections, discovery, and health checks.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
pub use kube::Client;
use kube::Discovery;
use kube::config::{Config, KubeConfigOptions, Kubeconfig};
use tokio::sync::{OnceCell, Semaphore, watch};
use tokio::task::JoinHandle;

use crate::discovery::ResourceEntry;
use crate::latency::{self, Latency, LatencyTracker};
use serde::{Deserialize, Serialize};

/// Interval for `/readyz` polling.
pub(crate) const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable cluster ID for persisted preferences.
///
/// The ID stays stable across processes and Rust versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterId(u64);

impl ClusterId {
    /// Derive an ID from the context name and server URL.
    pub fn derive(context: &str, server: &str) -> Self {
        Self(fnv1a(&[context.as_bytes(), b"\0", server.as_bytes()]))
    }

    /// Restore an ID from persisted data.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

/// Cluster health state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Ready,
    NotReady(String),
}

#[derive(Default)]
struct HealthAggregation {
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HealthTicket {
    generation: u64,
}

impl HealthAggregation {
    fn begin(&mut self) -> HealthTicket {
        self.generation = self.generation.saturating_add(1);
        HealthTicket {
            generation: self.generation,
        }
    }

    fn publish(&self, ticket: HealthTicket) -> bool {
        ticket.generation == self.generation
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    /// Nothing on this machine names a cluster.
    ///
    /// This is the arrival state of a brand new install and it is not a failure
    /// to read anything, so it does not borrow the read-failure sentence. It
    /// used to be `Kubeconfig(KubeconfigError::FindPath)` and came out as
    /// "Failed to read kubeconfig: failed to find the path of kubeconfig. Check
    /// the kubeconfig file and try again" — three claims, all wrong on a machine
    /// that has no kubeconfig: nothing was read, there is no file to check, and
    /// pressing the only offered control re-reads the same empty directory. The
    /// reader is told where a kubeconfig goes and what to do about it instead.
    #[error(
        "No kubeconfig was found. Clusters come from ~/.kube/config or the paths in KUBECONFIG. Create one, then reload kubeconfigs."
    )]
    NoKubeconfig,

    #[error(transparent)]
    KubeconfigSource(#[from] KubeconfigSourceError),

    /// Every path named was unreadable, which is a different problem from
    /// naming none: the reader has kubeconfigs and they are broken.
    ///
    /// The sources are summarised as `path: cause` rather than printed whole,
    /// because each one already ends in its own advice and the aggregate used to
    /// append a third copy of it, stranding a full stop between the two.
    #[error(
        "No kubeconfig could be read: {}. Check the paths above, then reload kubeconfigs.",
        .source_errors.iter().map(KubeconfigSourceError::summary).collect::<Vec<_>>().join("; ")
    )]
    KubeconfigSources {
        source_errors: Vec<KubeconfigSourceError>,
    },

    /// A file that was read, and that names no clusters at all.
    ///
    /// An empty `~/.kube/config` parses perfectly, so this used to arrive as an
    /// empty registry and from there as the session's "The selected context is no
    /// longer available. Reload Kubeconfigs and try again" — a sentence about a
    /// context that was never selected, pointing at a button that re-reads the
    /// same empty file and changes nothing. Naming the file is the one thing a
    /// reader can act on.
    #[error(
        "The kubeconfig at {sources} names no contexts, so there is nothing to connect to. A \
         context is one cluster's address and the credentials to reach it. Add one to the file, \
         then reload kubeconfigs."
    )]
    NoContexts { sources: String },

    /// Contexts the file names, and none of them could be turned into a client.
    ///
    /// The same dead end as [`Self::NoContexts`] with one step in front of it, and
    /// the fix differs: these contexts exist and are wrong rather than absent.
    #[error(
        "None of the {count} contexts in {sources} could be used: {contexts}. Fix the context and \
         cluster entries in kubeconfig, then reload kubeconfigs."
    )]
    NoUsableContexts {
        sources: String,
        count: usize,
        contexts: String,
    },

    #[error(
        "Context {context} is not usable. Fix the context and cluster entries in kubeconfig, then reload."
    )]
    Context {
        context: String,
        #[source]
        source: kube::config::KubeconfigError,
    },

    #[error(
        "Failed to create a client for cluster {cluster}: {source}. Check the cluster connection settings."
    )]
    Client {
        cluster: String,
        // Box keeps kube::Error below the Result size limit.
        #[source]
        source: Box<kube::Error>,
    },

    #[error(
        "Discovery failed for cluster {cluster}: {source}. Check the cluster connection and try again."
    )]
    Discovery {
        cluster: String,
        #[source]
        source: Box<kube::Error>,
    },

    #[error(
        "Context {context} did not finish loading within {seconds} seconds. A credential plugin or a slow API server is the usual cause. Check the context, then reload."
    )]
    ContextLoadTimeout { context: String, seconds: u64 },

    #[error("Failed to run kubeconfig loading off the async runtime: {0}")]
    Blocking(#[source] tokio::task::JoinError),
}

/// A context that failed to load. Other contexts remain available.
#[derive(Debug, thiserror::Error)]
#[error("Context {context} did not load. Fix kubeconfig, then try again.")]
pub struct ContextLoadError {
    pub context: String,
    #[source]
    pub source: ClusterError,
}

#[derive(Debug, thiserror::Error)]
#[error("Kubeconfig source {path}: {source}. Check the file and try again.")]
pub struct KubeconfigSourceError {
    pub path: PathBuf,
    #[source]
    pub source: kube::config::KubeconfigError,
}

impl KubeconfigSourceError {
    /// Where the file is and what went wrong, without this type's own advice.
    ///
    /// [`ClusterError::KubeconfigSources`] prints every source it could not
    /// read and then says what to do about all of them, so quoting this type's
    /// own sentence inside that one printed the same instruction once per file.
    pub fn summary(&self) -> String {
        format!("{}: {}", self.path.display(), self.source)
    }
}

/// A connected cluster with a built client. Connections remain lazy.
#[derive(Clone)]
pub struct Cluster {
    id: ClusterId,
    name: Arc<str>,
    client: Client,
    discovery: Arc<OnceCell<Discovery>>,
    health: watch::Sender<Health>,
    health_aggregation: Arc<Mutex<HealthAggregation>>,
    latency: watch::Sender<Latency>,
    probes: Arc<Mutex<LatencyTracker>>,
    background: Arc<Semaphore>,
}

impl Cluster {
    pub fn id(&self) -> ClusterId {
        self.id
    }

    /// Context name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Shared `kube::Client` for the cluster. Do not create one per view.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn search_resources_with_limit(
        &self,
        resource: &ResourceEntry,
        query: &str,
    ) -> Result<crate::controller::SearchOutcome, crate::controller::SearchError> {
        crate::controller::search_resources_with_limit(&self.client, resource, query).await
    }

    pub async fn search_cluster_resources(
        &self,
        resources: &[ResourceEntry],
        query: &str,
    ) -> Result<crate::controller::SearchOutcome, crate::controller::SearchError> {
        crate::controller::search_cluster_resources(&self.client, resources, query).await
    }

    /// Subscribe to health changes.
    pub fn health(&self) -> watch::Receiver<Health> {
        self.health.subscribe()
    }

    /// Subscribe to RTT and packet-loss tier changes.
    pub fn latency(&self) -> watch::Receiver<Latency> {
        self.latency.subscribe()
    }

    pub fn health_snapshot(&self) -> Health {
        self.health.borrow().clone()
    }

    pub fn latency_snapshot(&self) -> Latency {
        self.latency.borrow().clone()
    }

    /// Bounded semaphore shared by background requests.
    pub fn background_limiter(&self) -> Arc<Semaphore> {
        Arc::clone(&self.background)
    }

    /// Run one `/readyz` check without broadcasting the result.
    pub async fn check_health(&self) -> Health {
        let Ok(request) = http::Request::get("/readyz").body(Vec::new()) else {
            return Health::NotReady(
                "Unable to build the /readyz request. Check the client configuration.".to_string(),
            );
        };
        match tokio::time::timeout(
            latency::NON_WATCH_READ_TIMEOUT,
            self.client.request_text(request),
        )
        .await
        {
            Ok(Ok(body)) if body.trim() == "ok" => Health::Ready,
            Ok(Ok(body)) => Health::NotReady(body.trim().to_string()),
            Ok(Err(err)) => Health::NotReady(err.to_string()),
            Err(_) => Health::NotReady(
                "Health check timed out. Check the cluster connection and try again.".to_owned(),
            ),
        }
    }

    /// Check health and broadcast the result.
    pub async fn refresh_health(&self) -> Health {
        let ticket = self
            .health_aggregation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin();
        let health = self.check_health().await;
        let aggregation = self
            .health_aggregation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if aggregation.publish(ticket) {
            self.health.send_replace(health.clone());
            return health;
        }
        drop(aggregation);
        self.health_snapshot()
    }

    /// Measure `/version` latency. A failed request counts as packet loss.
    pub async fn probe_latency(&self) -> Latency {
        let sample = match http::Request::get("/version").body(Vec::new()) {
            Ok(request) => {
                let started = Instant::now();
                match tokio::time::timeout(
                    latency::NON_WATCH_READ_TIMEOUT,
                    self.client.request_text(request),
                )
                .await
                {
                    Ok(Ok(_)) => Some(started.elapsed()),
                    Ok(Err(_)) | Err(_) => None,
                }
            }
            Err(_) => None,
        };
        let snapshot = {
            let mut probes = self
                .probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            probes.record(sample);
            probes.snapshot()
        };
        if *self.latency.borrow() != snapshot {
            // send_replace keeps the latest value for future subscribers.
            self.latency.send_replace(snapshot.clone());
        }
        snapshot
    }

    /// Run full discovery on demand and cache a successful result.
    pub async fn discovery(&self) -> Result<&Discovery, ClusterError> {
        self.discovery
            .get_or_try_init(|| async {
                Discovery::new(self.client.clone())
                    .run()
                    .await
                    .map_err(|source| ClusterError::Discovery {
                        cluster: self.name.to_string(),
                        source: Box::new(source),
                    })
            })
            .await
    }
}

impl fmt::Debug for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cluster")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("discovery_ready", &self.discovery.initialized())
            .finish_non_exhaustive()
    }
}

const REGISTRY_MONITOR_CONCURRENCY: usize = 4;

#[derive(Default)]
struct MonitorSet {
    task: Option<JoinHandle<()>>,
}

impl MonitorSet {
    fn start(clusters: &[Cluster]) -> Self {
        let task = if clusters.is_empty() {
            None
        } else {
            tokio::runtime::Handle::try_current().ok().map(|runtime| {
                let clusters = clusters.to_vec();
                runtime.spawn(async move {
                    monitor_clusters(clusters).await;
                })
            })
        };
        Self { task }
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn monitor_clusters(clusters: Vec<Cluster>) {
    let mut health_ticker = tokio::time::interval(HEALTH_INTERVAL);
    let mut latency_ticker = tokio::time::interval(latency::PROBE_INTERVAL);
    health_ticker.tick().await;
    latency_ticker.tick().await;
    run_monitor_round(&clusters, true).await;
    run_monitor_round(&clusters, false).await;
    loop {
        tokio::select! {
            _ = health_ticker.tick() => {
                run_monitor_round(&clusters, false).await;
            }
            _ = latency_ticker.tick() => {
                run_monitor_round(&clusters, true).await;
            }
        }
    }
}

async fn run_monitor_round(clusters: &[Cluster], latency_probe: bool) {
    futures::stream::iter(clusters.iter().cloned())
        .for_each_concurrent(REGISTRY_MONITOR_CONCURRENCY, |cluster| async move {
            let Ok(_permit) = cluster.background_limiter().acquire_owned().await else {
                return;
            };
            if latency_probe {
                cluster.probe_latency().await;
            } else {
                cluster.refresh_health().await;
            }
        })
        .await;
}

impl fmt::Debug for MonitorSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonitorSet")
            .field("task_count", &self.task.as_ref().map_or(0, |_| 1))
            .finish()
    }
}

impl Drop for MonitorSet {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Registry of clusters from kubeconfig contexts.
#[derive(Debug, Default)]
pub struct ClusterRegistry {
    clusters: Vec<Cluster>,
    uid_cells: Mutex<HashMap<ClusterId, Arc<OnceCell<String>>>>,
    _monitors: MonitorSet,
    context_errors: Vec<ContextLoadError>,
    kubeconfig: Arc<Kubeconfig>,
    source_errors: Vec<KubeconfigSourceError>,
    context_sources: HashMap<String, PathBuf>,
    sources: Vec<PathBuf>,
}

fn read_kubeconfig_blocking(
    path: PathBuf,
) -> Result<(Kubeconfig, HashMap<String, PathBuf>), KubeconfigSourceError> {
    let kubeconfig = Kubeconfig::read_from(&path).map_err(|source| KubeconfigSourceError {
        path: path.clone(),
        source,
    })?;
    let context_sources = kubeconfig
        .contexts
        .iter()
        .map(|context| (context.name.clone(), path.clone()))
        .collect();
    Ok((kubeconfig, context_sources))
}

type LoadedKubeconfigSources = (
    Kubeconfig,
    Vec<KubeconfigSourceError>,
    HashMap<String, PathBuf>,
    Vec<PathBuf>,
);

fn load_sources_blocking(paths: Vec<PathBuf>) -> Result<LoadedKubeconfigSources, ClusterError> {
    if paths.is_empty() {
        return Err(ClusterError::NoKubeconfig);
    }

    let mut merged = Kubeconfig::default();
    let mut source_errors = Vec::new();
    let mut context_sources = HashMap::new();
    let mut sources = Vec::new();
    let mut loaded_paths = HashSet::new();
    let mut loaded_source = false;
    for path in paths {
        let path_key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if loaded_paths.contains(&path_key) {
            continue;
        }
        let source = match Kubeconfig::read_from(&path) {
            Ok(source) => source,
            Err(error) => {
                source_errors.push(KubeconfigSourceError {
                    path,
                    source: error,
                });
                continue;
            }
        };
        let new_context_sources = source
            .contexts
            .iter()
            .filter(|context| !context_sources.contains_key(&context.name))
            .map(|context| (context.name.clone(), path.clone()))
            .collect::<Vec<_>>();
        match merged.clone().merge(source) {
            Ok(next) => {
                loaded_source = true;
                loaded_paths.insert(path_key);
                sources.push(path);
                merged = next;
                context_sources.extend(new_context_sources);
            }
            Err(error) => source_errors.push(KubeconfigSourceError {
                path,
                source: error,
            }),
        }
    }

    if !loaded_source {
        return Err(ClusterError::KubeconfigSources { source_errors });
    }
    Ok((merged, source_errors, context_sources, sources))
}

impl ClusterRegistry {
    /// Load all contexts from a kubeconfig file.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, ClusterError> {
        let path = path.as_ref().to_path_buf();
        let read_path = path.clone();
        let (kubeconfig, context_sources) =
            tokio::task::spawn_blocking(move || read_kubeconfig_blocking(read_path))
                .await
                .map_err(ClusterError::Blocking)??;
        Ok(Self::from_loaded(
            Arc::new(kubeconfig),
            Vec::new(),
            context_sources,
            vec![path],
        )
        .await)
    }

    /// Load from `$KUBECONFIG` or `~/.kube/config`, and refuse an answer that
    /// names nothing to connect to.
    ///
    /// Only this entry point does that, and it is the one the app reaches for
    /// when it asks the machine what it has. [`Self::load`] and
    /// [`Self::load_sources`] stay what they say on the tin — a registry of
    /// whatever the named files contain, empty included — because a caller that
    /// names its own file is reading that file and can see what it got.
    pub async fn load_default() -> Result<Self, ClusterError> {
        let paths = tokio::task::spawn_blocking(crate::paths::default_kubeconfig_paths)
            .await
            .map_err(ClusterError::Blocking)?;
        let registry = Self::load_sources(paths).await?;
        registry.check_usable()?;
        Ok(registry)
    }

    /// Turns "the files were read" into "there is something to connect to".
    ///
    /// An empty registry is a perfectly good value for a struct that models a
    /// file, and a useless one for the answer the app asked for: the shell turns
    /// it into a session with no cluster, and every surface that has to say why
    /// then has to invent a reason from an absence. Both of the states below are
    /// arrival states a new person can hit on a first run — an empty file, or one
    /// naming only contexts that cannot be built — and each is reported with the
    /// file and the move, because the move is not the same for both.
    fn check_usable(&self) -> Result<(), ClusterError> {
        if !self.clusters.is_empty() {
            return Ok(());
        }
        let sources = if self.sources.is_empty() {
            "the kubeconfig".to_owned()
        } else {
            self.sources
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if self.kubeconfig.contexts.is_empty() {
            return Err(ClusterError::NoContexts { sources });
        }
        // Only the names, because every `ClusterError` here ends in its own
        // advice and a third copy of it helps nobody. The sidebar already lists
        // each failing context with its own reason beside it.
        let contexts = self
            .context_errors
            .iter()
            .map(|error| error.context.as_str())
            .collect::<Vec<_>>();
        Err(ClusterError::NoUsableContexts {
            sources,
            count: contexts.len(),
            contexts: contexts.join(", "),
        })
    }

    pub async fn load_sources(paths: Vec<PathBuf>) -> Result<Self, ClusterError> {
        let (merged, source_errors, context_sources, sources) =
            tokio::task::spawn_blocking(move || load_sources_blocking(paths))
                .await
                .map_err(ClusterError::Blocking)??;
        Ok(Self::from_loaded(Arc::new(merged), source_errors, context_sources, sources).await)
    }

    pub async fn from_kubeconfig(kubeconfig: Kubeconfig) -> Self {
        Self::from_loaded(Arc::new(kubeconfig), Vec::new(), HashMap::new(), Vec::new()).await
    }

    async fn from_loaded(
        kubeconfig: Arc<Kubeconfig>,
        source_errors: Vec<KubeconfigSourceError>,
        context_sources: HashMap<String, PathBuf>,
        sources: Vec<PathBuf>,
    ) -> Self {
        Self::from_loaded_within(
            kubeconfig,
            source_errors,
            context_sources,
            sources,
            latency::REGISTRY_LOAD_DEADLINE,
            latency::REGISTRY_TOTAL_DEADLINE,
        )
        .await
    }

    /// Loads every context, bounding each one and the whole loop.
    ///
    /// The budgets are parameters so a test can prove the bound without waiting it out.
    async fn from_loaded_within(
        kubeconfig: Arc<Kubeconfig>,
        source_errors: Vec<KubeconfigSourceError>,
        context_sources: HashMap<String, PathBuf>,
        sources: Vec<PathBuf>,
        context_budget: Duration,
        total_budget: Duration,
    ) -> Self {
        let mut clusters = Vec::with_capacity(kubeconfig.contexts.len());
        let mut context_errors = Vec::new();
        // One hung credential plugin must not take the registry with it, so every context gets
        // its own budget. A single shared deadline would be cheaper and worse: the first hang
        // would spend the whole budget and report every later context as broken too, including
        // the ones that are fine. Contexts load serially, so a file where they all hang needs a
        // total backstop as well.
        let total_deadline = tokio::time::Instant::now() + total_budget;

        for named in &kubeconfig.contexts {
            if tokio::time::Instant::now() >= total_deadline {
                context_errors.push(ContextLoadError {
                    context: named.name.clone(),
                    source: ClusterError::ContextLoadTimeout {
                        context: named.name.clone(),
                        seconds: total_budget.as_secs(),
                    },
                });
                continue;
            }
            let load = cluster_from_context(&kubeconfig, &named.name);
            let budget = tokio::time::Instant::now() + context_budget;
            // Whichever runs out first ends this context: its own budget, or the backstop.
            let (deadline, seconds) = if budget < total_deadline {
                (budget, context_budget.as_secs())
            } else {
                (total_deadline, total_budget.as_secs())
            };
            match tokio::time::timeout_at(deadline, load).await {
                Ok(Ok(cluster)) => clusters.push(cluster),
                Ok(Err(source)) => context_errors.push(ContextLoadError {
                    context: named.name.clone(),
                    source,
                }),
                Err(_) => context_errors.push(ContextLoadError {
                    context: named.name.clone(),
                    source: ClusterError::ContextLoadTimeout {
                        context: named.name.clone(),
                        seconds,
                    },
                }),
            }
        }

        let monitors = MonitorSet::start(&clusters);
        Self {
            clusters,
            uid_cells: Mutex::new(HashMap::new()),
            _monitors: monitors,
            context_errors,
            kubeconfig,
            source_errors,
            context_sources,
            sources,
        }
    }

    pub fn clusters(&self) -> &[Cluster] {
        &self.clusters
    }

    /// Contexts that failed to load.
    pub fn context_errors(&self) -> &[ContextLoadError] {
        &self.context_errors
    }

    pub fn source_errors(&self) -> &[KubeconfigSourceError] {
        &self.source_errors
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    pub fn context_source(&self, context: &str) -> Option<&Path> {
        self.context_sources.get(context).map(PathBuf::as_path)
    }

    pub fn kubeconfig(&self) -> Arc<Kubeconfig> {
        Arc::clone(&self.kubeconfig)
    }

    pub fn current_context(&self) -> Option<&str> {
        self.kubeconfig.current_context.as_deref()
    }

    pub fn current_cluster_id(&self) -> Option<ClusterId> {
        self.current_context()
            .and_then(|context| {
                self.clusters
                    .iter()
                    .find(|cluster| cluster.name() == context)
            })
            .map(Cluster::id)
    }

    pub fn get(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.iter().find(|cluster| cluster.id == id)
    }

    pub fn uid_cell(&self, id: ClusterId) -> Arc<OnceCell<String>> {
        let mut cells = self
            .uid_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(cells.entry(id).or_insert_with(|| Arc::new(OnceCell::new())))
    }
}

async fn cluster_from_context(
    kubeconfig: &Kubeconfig,
    context: &str,
) -> Result<Cluster, ClusterError> {
    let options = KubeConfigOptions {
        context: Some(context.to_string()),
        ..KubeConfigOptions::default()
    };
    let config = Config::from_custom_kubeconfig(kubeconfig.clone(), &options)
        .await
        .map_err(|source| ClusterError::Context {
            context: context.to_string(),
            source,
        })?;
    let server = config.cluster_url.to_string();
    // Client::try_from is synchronous, and building it runs the kubeconfig credential plugin as
    // a child process. On the async runtime that blocks the worker thread, so one plugin that
    // waits for input freezes every other task until it exits — and a timeout around this
    // function could never fire, because the runtime never got polled. A blocking pool thread
    // keeps the caller awaitable so the deadline can actually expire.
    let client = tokio::task::spawn_blocking(move || Client::try_from(tuned_transport(config)))
        .await
        .map_err(ClusterError::Blocking)?
        .map_err(|source| ClusterError::Client {
            cluster: context.to_string(),
            source: Box::new(source),
        })?;
    let (health, _receiver) = watch::channel(Health::Unknown);
    let (latency, _receiver) = watch::channel(Latency::default());

    Ok(Cluster {
        id: ClusterId::derive(context, &server),
        name: Arc::from(context),
        client,
        discovery: Arc::new(OnceCell::new()),
        health,
        health_aggregation: Arc::new(Mutex::new(HealthAggregation::default())),
        latency,
        probes: Arc::new(Mutex::new(LatencyTracker::default())),
        background: Arc::new(Semaphore::new(latency::BACKGROUND_CONCURRENCY)),
    })
}

/// Set transport timeouts while leaving watch reads under watcher control.
fn tuned_transport(mut config: Config) -> Config {
    config.connect_timeout = Some(latency::CONNECT_TIMEOUT);
    config.write_timeout = Some(latency::WRITE_TIMEOUT);
    config.default_retry = false;
    config
}

/// Check whether a kubeconfig source exists for tests.
#[cfg(test)]
pub(crate) fn kubeconfig_present() -> bool {
    std::env::var_os("KUBECONFIG").is_some()
        || std::env::var_os("HOME")
            .is_some_and(|home| Path::new(&home).join(".kube").join("config").exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_kubeconfig(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("k8s-gpui-{}-{}.yaml", std::process::id(), name));
        std::fs::write(&path, contents).expect("write temp kubeconfig");
        path
    }

    fn write_config(root: &Path, name: &str, contents: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, contents).expect("write temp kubeconfig");
        path
    }

    fn kubeconfig(entries: &[(&str, &str, &str)], current_context: &str) -> String {
        let clusters = entries
            .iter()
            .map(|(_, cluster, server)| {
                format!("- name: {cluster}\n  cluster:\n    server: {server}\n")
            })
            .collect::<String>();
        let contexts = entries
            .iter()
            .map(|(context, cluster, _)| {
                format!("- name: {context}\n  context:\n    cluster: {cluster}\n")
            })
            .collect::<String>();
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n{clusters}contexts:\n{contexts}current-context: {current_context}\n"
        )
    }

    const TWO_CONTEXTS: &str = r#"
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
  context:
    cluster: alpha
    user: alpha-user
- name: beta-ctx
  context:
    cluster: beta
    user: beta-user
users:
- name: alpha-user
  user: {}
- name: beta-user
  user: {}
current-context: alpha-ctx
"#;

    /// A credential plugin that never exits, standing in for a plugin waiting on input.
    #[cfg(unix)]
    fn hanging_plugin_kubeconfig() -> String {
        let script =
            std::env::temp_dir().join(format!("k8s-gpui-hanging-plugin-{}.sh", std::process::id()));
        // Long enough to outlast the 300ms budget this test uses, short enough that the plugin
        // cannot outlive the run: a leaked child keeps the harness output pipe open, and the
        // test binary then waits for it instead of exiting.
        std::fs::write(&script, "#!/bin/sh\nsleep 2\n").expect("write plugin");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("make plugin executable");
        format!(
            r#"
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
  context:
    cluster: alpha
    user: alpha-user
- name: beta-ctx
  context:
    cluster: beta
    user: beta-user
users:
- name: alpha-user
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: {}
- name: beta-user
  user: {{}}
current-context: alpha-ctx
"#,
            script.display()
        )
    }

    /// A hung credential plugin must fail the context instead of hanging the whole load.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_credential_plugin_fails_only_its_own_context() {
        let kubeconfig = Kubeconfig::from_yaml(&hanging_plugin_kubeconfig()).expect("parse");

        // A 300ms budget stands in for the 30s production one: the plugin sleeps far longer
        // than either, so the bound is what decides the outcome.
        let registry = ClusterRegistry::from_loaded_within(
            Arc::new(kubeconfig),
            Vec::new(),
            HashMap::new(),
            Vec::new(),
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(5),
        )
        .await;

        let hung = registry
            .context_errors()
            .iter()
            .find(|error| error.context == "alpha-ctx")
            .expect("the hung context is reported");
        assert!(
            matches!(hung.source, ClusterError::ContextLoadTimeout { .. }),
            "{:?}",
            hung.source
        );
        assert!(
            hung.source.to_string().contains("within 0 seconds"),
            "the message names the deadline: {}",
            hung.source
        );
        assert!(
            registry
                .clusters()
                .iter()
                .any(|cluster| cluster.name() == "beta-ctx"),
            "the healthy context still loads: {:?}",
            registry.context_errors()
        );
    }

    #[tokio::test]
    async fn missing_kubeconfig_reports_path() {
        let path = std::env::temp_dir().join("k8s-gpui-does-not-exist.yaml");
        let error = ClusterRegistry::load(&path)
            .await
            .expect_err("missing file must fail");
        assert!(matches!(&error, ClusterError::KubeconfigSource(_)));
        let display = error.to_string();
        assert!(display.contains("k8s-gpui-does-not-exist.yaml"));
        assert!(!display.contains("PathBuf"));
    }

    #[tokio::test]
    async fn invalid_yaml_kubeconfig_is_rejected() {
        let path = write_kubeconfig("invalid", "contexts: [unterminated");
        let error = ClusterRegistry::load(&path)
            .await
            .expect_err("bad yaml must fail");
        assert!(matches!(error, ClusterError::KubeconfigSource(_)));
    }

    #[tokio::test]
    async fn loads_every_context_offline() {
        let path = write_kubeconfig("two-contexts", TWO_CONTEXTS);
        let registry = ClusterRegistry::load(&path)
            .await
            .expect("kubeconfig loads without connecting");

        assert!(registry.context_errors().is_empty());
        assert_eq!(registry.sources(), vec![path.clone()]);
        let names: Vec<&str> = registry.clusters().iter().map(Cluster::name).collect();
        assert_eq!(names, ["alpha-ctx", "beta-ctx"]);

        let ids: Vec<ClusterId> = registry.clusters().iter().map(Cluster::id).collect();
        assert_ne!(ids[0], ids[1]);
        assert!(registry.get(ids[1]).is_some());
        let health = registry.clusters()[0].health();
        assert_eq!(&*health.borrow(), &Health::Unknown);
    }

    #[tokio::test]
    async fn merges_every_kubeconfig_path_in_order_with_first_wins() {
        let root = tempfile::tempdir().expect("temp directory");
        let first = write_config(
            root.path(),
            "first.yaml",
            &kubeconfig(
                &[("shared-ctx", "shared", "http://127.0.0.1:6443")],
                "shared-ctx",
            ),
        );
        let second = write_config(
            root.path(),
            "second.yml",
            &kubeconfig(
                &[
                    ("second-ctx", "second", "http://127.0.0.1:6444"),
                    ("shared-ctx", "shared", "http://127.0.0.1:9999"),
                ],
                "second-ctx",
            ),
        );
        let value = std::env::join_paths([&first, &second]).expect("join KUBECONFIG paths");
        let paths = crate::paths::kubeconfig_paths(Some(value), None);

        let registry = ClusterRegistry::load_sources(paths)
            .await
            .expect("merge configs");
        let snapshot = registry.kubeconfig();
        let same_snapshot = registry.kubeconfig();

        assert!(Arc::ptr_eq(&snapshot, &same_snapshot));
        assert_eq!(registry.sources(), vec![first.clone(), second.clone()]);
        assert_eq!(snapshot.current_context.as_deref(), Some("shared-ctx"));
        assert_eq!(
            snapshot
                .clusters
                .iter()
                .find(|cluster| cluster.name == "shared")
                .and_then(|cluster| cluster.cluster.as_ref())
                .and_then(|cluster| cluster.server.as_deref()),
            Some("http://127.0.0.1:6443")
        );
        assert_eq!(
            registry
                .clusters()
                .iter()
                .map(Cluster::name)
                .collect::<Vec<_>>(),
            ["shared-ctx", "second-ctx"]
        );
        assert_eq!(registry.context_source("shared-ctx"), Some(first.as_path()));
        assert_eq!(
            registry.context_source("second-ctx"),
            Some(second.as_path())
        );
    }

    #[tokio::test]
    async fn loads_context_user_and_cluster_from_different_sources() {
        let root = tempfile::tempdir().expect("temp directory");
        let context_path = write_config(
            root.path(),
            "contexts.yaml",
            r#"
apiVersion: v1
kind: Config
contexts:
- name: cross-file-ctx
  context:
    cluster: cross-cluster
    user: cross-user
current-context: cross-file-ctx
"#,
        );
        let resource_path = write_config(
            root.path(),
            "resources.yaml",
            r#"
apiVersion: v1
kind: Config
clusters:
- name: cross-cluster
  cluster:
    server: http://127.0.0.1:6443
users:
- name: cross-user
  user: {}
"#,
        );

        let registry = ClusterRegistry::load_sources(vec![
            context_path.clone(),
            resource_path.clone(),
            context_path.clone(),
        ])
        .await
        .expect("merge split kubeconfig");
        let snapshot = registry.kubeconfig();

        assert_eq!(
            registry.sources(),
            vec![context_path.clone(), resource_path.clone()]
        );
        assert_eq!(snapshot.contexts.len(), 1);
        assert_eq!(snapshot.auth_infos.len(), 1);
        assert_eq!(snapshot.clusters.len(), 1);
        assert_eq!(snapshot.contexts[0].name, "cross-file-ctx");
        assert_eq!(snapshot.auth_infos[0].name, "cross-user");
        assert_eq!(snapshot.clusters[0].name, "cross-cluster");
        assert_eq!(
            snapshot.contexts[0]
                .context
                .as_ref()
                .map(|context| context.cluster.as_str()),
            Some("cross-cluster")
        );
        assert_eq!(
            snapshot.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.user.as_deref()),
            Some("cross-user")
        );
        assert!(snapshot.auth_infos[0].auth_info.is_some());
        assert_eq!(
            snapshot.clusters[0]
                .cluster
                .as_ref()
                .and_then(|cluster| cluster.server.as_deref()),
            Some("http://127.0.0.1:6443")
        );
        assert_eq!(registry.clusters().len(), 1);
        assert!(registry.context_errors().is_empty());
    }

    #[tokio::test]
    async fn malformed_second_file_is_isolated_with_its_path() {
        let root = tempfile::tempdir().expect("temp directory");
        let first = write_config(
            root.path(),
            "first.yaml",
            &kubeconfig(
                &[("first-ctx", "first", "http://127.0.0.1:6443")],
                "first-ctx",
            ),
        );
        let second = write_config(root.path(), "second.yaml", "contexts: [unterminated");
        let value = std::env::join_paths([&first, &second]).expect("join KUBECONFIG paths");
        let paths = crate::paths::kubeconfig_paths(Some(value), None);

        let registry = ClusterRegistry::load_sources(paths)
            .await
            .expect("retain valid source");

        assert_eq!(registry.sources(), vec![first.clone()]);
        assert_eq!(
            registry
                .clusters()
                .iter()
                .map(Cluster::name)
                .collect::<Vec<_>>(),
            ["first-ctx"]
        );
        assert_eq!(registry.source_errors().len(), 1);
        assert_eq!(registry.source_errors()[0].path, second);
        assert!(
            registry.source_errors()[0]
                .to_string()
                .contains("second.yaml")
        );
    }

    #[tokio::test]
    async fn all_failed_sources_return_readable_errors() {
        let root = tempfile::tempdir().expect("temp directory");
        let first = write_config(root.path(), "first.yaml", "contexts: [unterminated");
        let second = write_config(root.path(), "second.yaml", "clusters: [unterminated");

        let error = ClusterRegistry::load_sources(vec![first.clone(), second.clone()])
            .await
            .expect_err("all sources failed");
        let source_errors = match &error {
            ClusterError::KubeconfigSources { source_errors } => source_errors,
            _ => panic!("aggregate source errors"),
        };

        assert_eq!(source_errors.len(), 2);
        assert_eq!(source_errors[0].path, first);
        assert_eq!(source_errors[1].path, second);
        let reported = error.to_string();
        assert!(reported.contains("first.yaml"));
        assert!(reported.contains("second.yaml"));
        assert_eq!(
            reported.matches("reload kubeconfigs").count(),
            1,
            "each source used to carry its own advice and the aggregate a third, so one \
             unopenable path printed the instruction three times: {reported}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_second_source_is_isolated_with_its_path() {
        let root = tempfile::tempdir().expect("temp directory");
        let first = write_config(
            root.path(),
            "first.yaml",
            &kubeconfig(
                &[("first-ctx", "first", "http://127.0.0.1:6443")],
                "first-ctx",
            ),
        );
        let second = root.path().join("second.yaml");
        std::fs::create_dir(&second).expect("create unreadable source");

        let registry = ClusterRegistry::load_sources(vec![first.clone(), second.clone()])
            .await
            .expect("retain valid source");

        assert_eq!(registry.sources(), vec![first]);
        assert_eq!(registry.clusters().len(), 1);
        assert_eq!(registry.source_errors().len(), 1);
        assert_eq!(registry.source_errors()[0].path, second);
    }

    #[tokio::test]
    async fn merged_current_context_can_resolve_a_later_cluster() {
        let root = tempfile::tempdir().expect("temp directory");
        let first = write_config(
            root.path(),
            "first.yaml",
            &kubeconfig(
                &[("first-ctx", "first", "http://127.0.0.1:6443")],
                "second-ctx",
            ),
        );
        let second = write_config(
            root.path(),
            "second.yaml",
            &kubeconfig(
                &[("second-ctx", "second", "http://127.0.0.1:6444")],
                "second-ctx",
            ),
        );

        let registry = ClusterRegistry::load_sources(vec![first, second.clone()])
            .await
            .expect("merge configs");
        let second_id = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == "second-ctx")
            .map(Cluster::id)
            .expect("second context");

        assert_eq!(registry.current_context(), Some("second-ctx"));
        assert_eq!(registry.current_cluster_id(), Some(second_id));
        assert_eq!(
            registry.context_source("second-ctx"),
            Some(second.as_path())
        );
    }

    #[tokio::test]
    async fn empty_config_retains_an_empty_snapshot() {
        let root = tempfile::tempdir().expect("temp directory");
        let path = write_config(root.path(), "empty.yaml", "");

        let registry = ClusterRegistry::load_sources(vec![path.clone()])
            .await
            .expect("load empty config");
        let snapshot = registry.kubeconfig();

        assert!(registry.clusters().is_empty());
        assert_eq!(registry.sources(), vec![path.clone()]);
        assert!(registry.source_errors().is_empty());
        assert!(registry.current_context().is_none());
        assert!(registry.current_cluster_id().is_none());
        assert!(snapshot.contexts.is_empty());
        assert!(snapshot.current_context.is_none());
    }

    /// A file that names no cluster is an arrival state the app can say out
    /// loud, not an empty value to hand back and make the UI explain.
    #[tokio::test]
    async fn a_kubeconfig_naming_no_contexts_is_reported_not_handed_over_empty() {
        let root = tempfile::tempdir().expect("temp directory");
        let path = write_config(root.path(), "empty.yaml", "");

        let registry = ClusterRegistry::load_sources(vec![path.clone()])
            .await
            .expect("the file itself reads");
        let error = registry
            .check_usable()
            .expect_err("nothing in the file can be connected to");

        assert!(matches!(error, ClusterError::NoContexts { .. }));
        let reported = error.to_string();
        assert!(
            reported.contains("empty.yaml"),
            "the reader has to be told which file to open: {reported}"
        );
    }

    /// Contexts that are all broken is the same dead end with a different fix,
    /// so the error names them rather than saying nothing is usable.
    #[tokio::test]
    async fn contexts_that_all_fail_to_load_are_named_in_the_error() {
        let contents = r#"
apiVersion: v1
kind: Config
clusters: []
contexts:
- name: orphan-ctx
  context:
    cluster: nowhere
    user: nobody
users: []
current-context: orphan-ctx
"#;
        let path = write_kubeconfig("all-broken", contents);

        let registry = ClusterRegistry::load(&path).await.expect("the file parses");
        let error = registry
            .check_usable()
            .expect_err("no context in the file can be connected to");

        assert!(matches!(error, ClusterError::NoUsableContexts { .. }));
        let reported = error.to_string();
        assert!(reported.contains("orphan-ctx"), "{reported}");
        assert_eq!(
            reported
                .matches("Fix the context and cluster entries")
                .count(),
            1,
            "the aggregate says the fix once. It used to be able to repeat one per context \
             because each context's own sentence was quoted whole: {reported}"
        );
    }

    #[tokio::test]
    async fn broken_context_is_skipped_and_recorded() {
        let contents = r#"
apiVersion: v1
kind: Config
clusters:
- name: good
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: good-ctx
  context:
    cluster: good
    user: good-user
- name: broken-ctx
  context:
    cluster: nowhere
    user: good-user
users:
- name: good-user
  user: {}
current-context: good-ctx
"#;
        let path = write_kubeconfig("broken-context", contents);
        let registry = ClusterRegistry::load(&path)
            .await
            .expect("file itself parses");

        let names: Vec<&str> = registry.clusters().iter().map(Cluster::name).collect();
        assert_eq!(names, ["good-ctx"]);
        assert_eq!(registry.context_errors().len(), 1);
        assert_eq!(registry.context_errors()[0].context, "broken-ctx");
        assert!(matches!(
            registry.context_errors()[0].source,
            ClusterError::Context { .. }
        ));
    }

    #[test]
    fn context_display_hides_source_but_keeps_debug_detail() {
        use std::error::Error;

        let error = ContextLoadError {
            context: "broken-ctx".to_owned(),
            source: ClusterError::Context {
                context: "broken-ctx".to_owned(),
                source: kube::config::KubeconfigError::LoadContext(
                    "private kubeconfig detail".to_owned(),
                ),
            },
        };
        let display = error.to_string();
        assert!(!display.contains("private kubeconfig detail"));
        assert!(display.contains("Fix kubeconfig"));
        assert!(format!("{error:?}").contains("private kubeconfig detail"));
        assert!(error.source().is_some());
        assert!(error.source().unwrap().source().is_some());
    }

    #[test]
    fn cluster_id_is_stable_and_distinct() {
        let first = ClusterId::derive("ctx", "https://api.example.com:6443");
        let same = ClusterId::derive("ctx", "https://api.example.com:6443");
        let other_server = ClusterId::derive("ctx", "https://other.example.com:6443");
        let other_context = ClusterId::derive("ctx2", "https://api.example.com:6443");

        assert_eq!(first, same);
        assert_ne!(first, other_server);
        assert_ne!(first, other_context);
    }

    #[test]
    fn only_the_newest_health_generation_publishes() {
        let mut aggregation = HealthAggregation::default();
        let older = aggregation.begin();
        let newer = aggregation.begin();

        assert!(!aggregation.publish(older));
        assert!(aggregation.publish(newer));
        assert!(
            !aggregation.publish(older),
            "a late result must not overwrite the newest state"
        );
    }

    async fn test_cluster() -> Arc<Cluster> {
        let kubeconfig = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse kubeconfig");
        let cluster = cluster_from_context(&kubeconfig, "alpha-ctx")
            .await
            .expect("build cluster offline");
        Arc::new(cluster)
    }

    #[test]
    fn transport_timeouts_are_capped_and_read_timeout_stays_unset() {
        let url = "http://127.0.0.1:6443".parse().expect("valid URL");
        let tuned = tuned_transport(Config::new(url));
        assert_eq!(tuned.connect_timeout, Some(latency::CONNECT_TIMEOUT));
        assert_eq!(tuned.write_timeout, Some(latency::WRITE_TIMEOUT));
        assert_eq!(
            tuned.read_timeout, None,
            "socket read timeout stops idle watches, so watcher timeout remains in control"
        );
        assert!(
            !tuned.disable_compression,
            "gzip stays enabled by default, and kubeconfig can override it"
        );
    }

    #[tokio::test]
    async fn kubeconfig_default_keeps_compression_enabled() {
        let kubeconfig = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse kubeconfig");
        let options = KubeConfigOptions {
            context: Some("alpha-ctx".to_string()),
            ..KubeConfigOptions::default()
        };
        let config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .expect("resolve context");
        assert!(!config.disable_compression);
    }

    #[tokio::test]
    async fn background_concurrency_is_capped_at_four() {
        let cluster = test_cluster().await;
        let limiter = cluster.background_limiter();
        let held: Vec<_> = (0..latency::BACKGROUND_CONCURRENCY)
            .map(|_| {
                limiter
                    .try_acquire()
                    .expect("the first four permits are available")
            })
            .collect();
        assert!(
            limiter.try_acquire().is_err(),
            "the fifth background request must wait"
        );
        drop(held);
        assert!(limiter.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn unreachable_cluster_is_recorded_as_loss_and_high_latency() {
        let cluster = test_cluster().await;
        let latency = cluster.probe_latency().await;
        assert_eq!(latency.rtt, None);
        assert_eq!((latency.probes, latency.failures), (1, 1));
        assert_eq!(latency.tier(), latency::LatencyTier::HighLatency);
        assert_eq!(
            &*cluster.latency().borrow(),
            &latency,
            "the result is broadcast to subscribers"
        );
    }

    #[tokio::test]
    async fn registry_uses_one_monitor_task_and_probes_immediately() {
        let path = write_kubeconfig("monitor-lifecycle", TWO_CONTEXTS);
        let registry = ClusterRegistry::load(&path)
            .await
            .expect("kubeconfig loads");
        assert!(registry._monitors.task.is_some());
        let alpha = registry.clusters().first().expect("alpha context").id();
        let mut latency = registry.get(alpha).expect("alpha cluster").latency();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if latency.borrow_and_update().probes > 0 {
                    break;
                }
                latency.changed().await.expect("latency sender");
            }
        })
        .await
        .expect("initial latency probe");
        let handle = registry
            ._monitors
            .task
            .as_ref()
            .expect("monitor task")
            .abort_handle();
        drop(registry);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    #[tokio::test]
    #[ignore = "Requires a kind cluster: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn probes_real_cluster_rtt() {
        if !kubeconfig_present() {
            return;
        }
        let registry = ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let latency = cluster.probe_latency().await;
        assert!(
            latency.rtt.is_some(),
            "the local kind cluster has an RTT: {latency:?}"
        );
        assert_eq!(latency.probes, 1);
        assert_eq!(cluster.latency().borrow().rtt, latency.rtt);
    }
}
