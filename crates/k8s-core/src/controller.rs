//! Runs one watcher and reflector for each cluster, GVR, and namespace.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::Client;
use kube::api::{Api, ApiResource, ListParams};
use kube::core::DynamicObject;
use kube::runtime::watcher;
use kube::runtime::watcher::InitialListStrategy;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::discovery::ResourceEntry;
use crate::fuzzy;
use crate::latency::LatencyTier;
use crate::machines::SearchHit;

/// Page size used by client-go.
pub const WATCH_PAGE_SIZE: u32 = 500;

/// Bounded event buffer. A slow consumer applies backpressure to the watcher.
pub const EVENT_BUFFER: usize = 1024;

pub const SEARCH_LIST_PAGE_SIZE: u32 = 500;
pub const SEARCH_LIST_LIMIT: usize = 5_000;
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
pub const SEARCH_GLOBAL_LIST_LIMIT: usize = 500;
pub const SEARCH_GLOBAL_CONCURRENCY: usize = 4;
pub const SEARCH_GLOBAL_SOURCE_TIMEOUT: Duration = Duration::from_secs(4);
pub const SEARCH_GLOBAL_TIMEOUT: Duration = Duration::from_secs(8);

const RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Watch scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// All namespaces or cluster-scoped resources.
    All,
    Namespace(String),
}

/// Watch selectors passed to the API server.
#[derive(Clone, Debug, Default)]
pub struct WatchOptions {
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    /// Transport tier. High latency uses streaming lists and a longer watch timeout.
    pub tier: LatencyTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOp {
    Apply,
    Delete,
}

/// Event sent to the projection layer. The object is shared through `Arc`.
#[derive(Clone, Debug)]
pub struct StoreEvent {
    pub op: StoreOp,
    pub obj: Arc<DynamicObject>,
}

/// Controller events with initial-list boundaries.
///
/// A retry can produce another `Init`. Consumers discard the previous partial initial list.
#[derive(Clone, Debug)]
pub enum ControllerEvent {
    /// A new initial list starts.
    Init,
    /// An object from the initial list.
    InitApply(Arc<DynamicObject>),
    /// The initial list ends. The store now has a complete view.
    InitDone,
    /// An incremental change after the initial list.
    Store(StoreEvent),
}

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error(
        "A Tokio runtime is required to start the watcher. Run this operation inside a Tokio runtime."
    )]
    NoRuntime,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Kubernetes resource search failed: {0}. Check the cluster connection and try again.")]
    Api(#[source] Box<kube::Error>),
    #[error("Kubernetes resource search timed out. Retry after checking the cluster connection.")]
    Timeout,
    #[error("Kubernetes rejected the credentials for this cluster.")]
    Unauthorized,
    #[error("The current identity cannot list resources in this cluster.")]
    Forbidden,
    #[error(
        "No listable Kubernetes resource types are available. Refresh resource discovery, then retry."
    )]
    NoResources,
    #[error("Kubernetes resource types were not searched.")]
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    pub scanned: usize,
    pub truncated: bool,
    pub partial: bool,
}

/// Watch for one open view. Dropping the controller aborts the task.
pub struct Controller {
    tier: LatencyTier,
    watch_task: JoinHandle<()>,
}

impl Controller {
    /// Start a watcher and reflector with initial-list boundaries.
    pub fn spawn_with_events(
        client: Client,
        resource: ApiResource,
        scope: Scope,
        options: WatchOptions,
    ) -> Result<(Self, mpsc::Receiver<ControllerEvent>), ControllerError> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ControllerError::NoRuntime)?;
        let (api, config) = prepare(client, &resource, &scope, &options);
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let watch_task = runtime.spawn(watch_loop(api, config, options.tier, EventSink { events }));
        Ok((
            Self {
                tier: options.tier,
                watch_task,
            },
            receiver,
        ))
    }

    pub fn tier(&self) -> LatencyTier {
        self.tier
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.watch_task.abort();
    }
}

pub fn looks_like_complete_resource_name(query: &str) -> bool {
    let query = query.trim();
    query.len() >= 12
        && query.len() <= 253
        && query.split('.').all(|segment| {
            !segment.is_empty()
                && segment.len() <= 63
                && segment
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment
                    .as_bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

pub fn name_field_selector(query: &str) -> Option<String> {
    let query = query.trim();
    looks_like_complete_resource_name(query).then(|| format!("metadata.name={query}"))
}

pub fn rank_search_candidates(
    resource: &ResourceEntry,
    candidates: impl IntoIterator<Item = DynamicObject>,
    query: &str,
) -> Vec<SearchHit> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut candidates = candidates
        .into_iter()
        .filter(|object| object.metadata.name.is_some())
        .map(|object| {
            Some(SearchHit {
                resource: resource.clone(),
                object: Arc::new(object),
            })
        })
        .collect::<Vec<_>>();
    let names = candidates
        .iter()
        .filter_map(|candidate| candidate.as_ref()?.object.metadata.name.as_deref())
        .collect::<Vec<_>>();
    fuzzy::rank(&query, names)
        .into_iter()
        .filter_map(|ranked| candidates.get_mut(ranked.index)?.take())
        .take(crate::machines::SEARCH_RESULT_LIMIT)
        .collect()
}

async fn list_search_candidates(
    api: &Api<DynamicObject>,
    limit: usize,
) -> Result<(Vec<DynamicObject>, usize, bool), SearchError> {
    let mut params = ListParams::default().limit(SEARCH_LIST_PAGE_SIZE);
    let mut candidates = Vec::with_capacity(limit);
    let mut scanned = 0;
    let mut previous_token = None;

    loop {
        let list = api.list(&params).await.map_err(search_error)?;
        let page_len = list.items.len();
        let remaining = limit.saturating_sub(scanned);
        let take = remaining.min(page_len);
        candidates.extend(list.items.into_iter().take(take));
        scanned += take;

        if scanned >= limit {
            let truncated = page_len > take
                || list
                    .metadata
                    .continue_
                    .as_deref()
                    .is_some_and(|token| !token.is_empty());
            return Ok((candidates, scanned, truncated));
        }

        let Some(token) = list.metadata.continue_.filter(|token| !token.is_empty()) else {
            return Ok((candidates, scanned, false));
        };
        if previous_token.as_deref() == Some(token.as_str()) {
            return Ok((candidates, scanned, true));
        }
        previous_token = Some(token.clone());
        params = ListParams::default()
            .limit(SEARCH_LIST_PAGE_SIZE)
            .continue_token(&token);
    }
}

fn search_error(error: kube::Error) -> SearchError {
    match error {
        kube::Error::Api(response) if response.code == 401 => SearchError::Unauthorized,
        kube::Error::Api(response) if response.code == 403 => SearchError::Forbidden,
        error => SearchError::Api(Box::new(error)),
    }
}

fn supports_exact_fallback(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 400 || response.code == 422)
}

fn rank_search_hits(candidates: Vec<SearchHit>, query: &str) -> Vec<SearchHit> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut candidates = candidates.into_iter().map(Some).collect::<Vec<_>>();
    let names = candidates
        .iter()
        .filter_map(|candidate| candidate.as_ref()?.object.metadata.name.as_deref())
        .collect::<Vec<_>>();
    fuzzy::rank(&query, names)
        .into_iter()
        .filter_map(|ranked| candidates.get_mut(ranked.index)?.take())
        .take(crate::machines::SEARCH_RESULT_LIMIT)
        .collect()
}

pub async fn search_resources_with_limit(
    client: &Client,
    resource: &ResourceEntry,
    query: &str,
) -> Result<SearchOutcome, SearchError> {
    tokio::time::timeout(
        SEARCH_TIMEOUT,
        search_resource_with_limit_inner(client, resource, query, SEARCH_LIST_LIMIT),
    )
    .await
    .map_err(|_| SearchError::Timeout)?
}

async fn search_resource_with_limit_inner(
    client: &Client,
    resource: &ResourceEntry,
    query: &str,
    limit: usize,
) -> Result<SearchOutcome, SearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(SearchOutcome {
            hits: Vec::new(),
            scanned: 0,
            truncated: false,
            partial: false,
        });
    }

    let api_resource = resource.to_api_resource();
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &api_resource);
    if let Some(selector) = name_field_selector(query) {
        let params = ListParams::default().fields(&selector);
        match api.list(&params).await {
            Ok(list) => {
                let scanned = list.items.len().min(limit);
                let truncated = list.items.len() > scanned;
                let hits = rank_search_candidates(
                    resource,
                    list.items
                        .into_iter()
                        .take(scanned)
                        .filter(|object| object.metadata.name.as_deref() == Some(query)),
                    query,
                );
                return Ok(SearchOutcome {
                    hits,
                    scanned,
                    truncated,
                    partial: false,
                });
            }
            Err(error) if supports_exact_fallback(&error) => {
                tracing::debug!(
                    kind = %resource.kind,
                    query,
                    %error,
                    "exact resource-name search is unsupported. Falling back to bounded client filtering"
                );
            }
            Err(error) => return Err(search_error(error)),
        }
    }

    let (candidates, scanned, truncated) = list_search_candidates(&api, limit).await?;
    let hits = rank_search_candidates(resource, candidates, query);
    if truncated {
        tracing::info!(
            kind = %resource.kind,
            scanned,
            limit,
            "resource search reached the candidate limit"
        );
    }
    Ok(SearchOutcome {
        hits,
        scanned,
        truncated,
        partial: false,
    })
}

pub async fn search_cluster_resources(
    client: &Client,
    resources: &[ResourceEntry],
    query: &str,
) -> Result<SearchOutcome, SearchError> {
    let resources = resources
        .iter()
        .filter(|resource| !resource.plural.contains('/') && resource.supports("list"))
        .cloned()
        .collect::<Vec<_>>();
    if resources.is_empty() {
        return Err(SearchError::NoResources);
    }
    let query = query.trim();
    if query.is_empty() {
        return Ok(SearchOutcome {
            hits: Vec::new(),
            scanned: 0,
            truncated: false,
            partial: false,
        });
    }

    let requests = resources.iter().cloned().map(|resource| {
        let client = client.clone();
        let query = query.to_owned();
        async move {
            tokio::time::timeout(
                SEARCH_GLOBAL_SOURCE_TIMEOUT,
                search_resource_with_limit_inner(
                    &client,
                    &resource,
                    &query,
                    SEARCH_GLOBAL_LIST_LIMIT,
                ),
            )
            .await
            .map_err(|_| SearchError::Timeout)?
        }
    });
    let mut requests = futures::stream::iter(requests).buffer_unordered(SEARCH_GLOBAL_CONCURRENCY);
    let deadline = tokio::time::sleep(SEARCH_GLOBAL_TIMEOUT);
    futures::pin_mut!(deadline);
    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut completed = 0usize;
    let mut forbidden = 0usize;
    let mut unauthorized = 0usize;
    let mut timed_out = 0usize;
    let mut failed = 0usize;
    let mut seen = 0usize;

    loop {
        let next = tokio::select! {
            _ = &mut deadline => None,
            next = requests.next() => next,
        };
        let Some(result) = next else {
            timed_out += resources.len().saturating_sub(seen);
            break;
        };
        seen += 1;
        match result {
            Ok(outcome) => {
                completed += 1;
                scanned = scanned.saturating_add(outcome.scanned);
                truncated |= outcome.truncated;
                candidates.extend(outcome.hits);
            }
            Err(SearchError::Forbidden) => forbidden += 1,
            Err(SearchError::Unauthorized) => unauthorized += 1,
            Err(SearchError::Timeout) => timed_out += 1,
            Err(SearchError::NoResources | SearchError::Unavailable | SearchError::Api(_)) => {
                failed += 1;
            }
        }
    }

    if completed == 0 {
        if forbidden == resources.len() {
            return Err(SearchError::Forbidden);
        }
        if unauthorized == resources.len() {
            return Err(SearchError::Unauthorized);
        }
        if timed_out == resources.len() {
            return Err(SearchError::Timeout);
        }
        return Err(SearchError::Unavailable);
    }

    let total_scanned = scanned;
    let scanned = total_scanned.min(SEARCH_LIST_LIMIT);
    let truncated = truncated || total_scanned > SEARCH_LIST_LIMIT;
    Ok(SearchOutcome {
        hits: rank_search_hits(candidates, query),
        scanned,
        truncated,
        partial: forbidden + unauthorized + timed_out + failed > 0,
    })
}

fn prepare(
    client: Client,
    resource: &ApiResource,
    scope: &Scope,
    options: &WatchOptions,
) -> (Api<DynamicObject>, watcher::Config) {
    let api = match scope {
        Scope::All => Api::all_with(client, resource),
        Scope::Namespace(namespace) => Api::namespaced_with(client, namespace, resource),
    };
    (api, watcher_config(&watcher::Config::default(), options))
}

fn watcher_config(base: &watcher::Config, options: &WatchOptions) -> watcher::Config {
    let mut config = base
        .clone()
        // Keep bookmarks enabled. kube 4.2 enables them by default.
        .page_size(WATCH_PAGE_SIZE)
        .timeout(options.tier.watch_timeout_secs());
    if options.tier.uses_streaming_lists() {
        config = config.streaming_lists();
    }
    if let Some(labels) = &options.label_selector {
        config = config.labels(labels);
    }
    if let Some(fields) = &options.field_selector {
        config = config.fields(fields);
    }
    config
}

#[derive(Debug)]
struct EventSink {
    events: mpsc::Sender<ControllerEvent>,
}

impl EventSink {
    async fn send_init(&mut self) -> bool {
        self.events.send(ControllerEvent::Init).await.is_ok()
    }

    async fn send_event(&mut self, event: watcher::Event<DynamicObject>) -> bool {
        let event = match event {
            watcher::Event::Init => ControllerEvent::Init,
            watcher::Event::InitApply(obj) => ControllerEvent::InitApply(Arc::new(obj)),
            watcher::Event::InitDone => ControllerEvent::InitDone,
            watcher::Event::Apply(obj) => ControllerEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj: Arc::new(obj),
            }),
            watcher::Event::Delete(obj) => ControllerEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj: Arc::new(obj),
            }),
        };
        self.events.send(event).await.is_ok()
    }
}

/// Initial list strategy. High latency tries streaming first, then falls back to ListWatch.
#[derive(Debug)]
struct ListStrategy {
    streaming: bool,
    fallback_used: bool,
    init_pending: bool,
    reconnecting: bool,
    backoff: ReconnectBackoff,
}

impl ListStrategy {
    fn new(tier: LatencyTier) -> Self {
        Self {
            streaming: tier.uses_streaming_lists(),
            fallback_used: false,
            init_pending: true,
            reconnecting: false,
            backoff: ReconnectBackoff::default(),
        }
    }

    fn config(&self, base: &watcher::Config) -> watcher::Config {
        let mut config = base.clone();
        config.initial_list_strategy = if self.streaming {
            InitialListStrategy::StreamingList
        } else {
            InitialListStrategy::ListWatch
        };
        config
    }

    fn take_init_boundary(&mut self) -> bool {
        std::mem::take(&mut self.init_pending)
    }

    fn arm_init(&mut self) {
        self.init_pending = true;
    }

    /// Return true to rebuild the watcher after the first streaming-list failure.
    fn on_error(&mut self, init_done: bool) -> bool {
        if self.reconnecting || !self.streaming || init_done || self.fallback_used {
            return false;
        }
        self.fallback_used = true;
        self.streaming = false;
        self.arm_init();
        tracing::warn!("streaming list initial sync failed. Falling back to ListWatch.");
        true
    }

    fn on_init_done(&mut self) {
        self.reconnecting = false;
        self.init_pending = false;
        self.backoff.reset();
    }

    fn start_reconnect(&mut self) {
        self.reconnecting = true;
        self.arm_init();
    }

    fn continue_reconnect(&mut self) {
        self.reconnecting = true;
        self.arm_init();
    }

    fn reconnect_delay(&mut self) -> Duration {
        self.backoff.next()
    }
}

#[derive(Debug, Default)]
struct ReconnectBackoff {
    failures: u8,
}

impl ReconnectBackoff {
    fn next(&mut self) -> Duration {
        let exponent = self.failures.min(5);
        let delay = Duration::from_secs(RECONNECT_BACKOFF_BASE.as_secs() << exponent)
            .min(RECONNECT_BACKOFF_MAX);
        self.failures = self.failures.saturating_add(1);
        delay
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drive {
    Rebuild,
    Reconnect,
    Closed,
}

async fn watch_loop(
    api: Api<DynamicObject>,
    base: watcher::Config,
    tier: LatencyTier,
    mut sink: EventSink,
) {
    let mut strategy = ListStrategy::new(tier);
    loop {
        let synthetic_init = strategy.take_init_boundary();
        let stream = watcher::watcher(api.clone(), strategy.config(&base));
        if synthetic_init && !sink.send_init().await {
            break;
        }
        match drive_stream_with_boundary(stream, &mut sink, &mut strategy, synthetic_init).await {
            Drive::Rebuild | Drive::Reconnect => {
                strategy.arm_init();
                wait_for_reconnect(&mut strategy).await;
            }
            Drive::Closed => break,
        }
    }
}

async fn wait_for_reconnect(strategy: &mut ListStrategy) {
    tokio::time::sleep(strategy.reconnect_delay()).await;
}

async fn drive_stream_with_boundary<S>(
    stream: S,
    sink: &mut EventSink,
    strategy: &mut ListStrategy,
    synthetic_init: bool,
) -> Drive
where
    S: futures::Stream<Item = watcher::Result<watcher::Event<DynamicObject>>>,
{
    futures::pin_mut!(stream);
    let mut init_done = false;
    let reconnecting = strategy.reconnecting;
    let mut suppress_watcher_init = synthetic_init;
    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                let is_init = matches!(&event, &watcher::Event::Init);
                let is_init_done = matches!(&event, &watcher::Event::InitDone);
                if is_init_done {
                    init_done = true;
                    strategy.on_init_done();
                }
                if is_init && suppress_watcher_init {
                    suppress_watcher_init = false;
                    continue;
                }
                suppress_watcher_init = false;
                if !sink.send_event(event).await {
                    return Drive::Closed;
                }
            }
            Err(error) => {
                if strategy.on_error(init_done) {
                    return Drive::Rebuild;
                }
                if init_done {
                    strategy.start_reconnect();
                    tracing::warn!(%error, "watch failed after initialization. Reconnecting");
                    return Drive::Reconnect;
                }
                if reconnecting {
                    strategy.continue_reconnect();
                } else {
                    strategy.arm_init();
                }
                return Drive::Rebuild;
            }
        }
    }
    if init_done {
        strategy.start_reconnect();
        tracing::warn!("watch stream ended after initialization. Reconnecting");
        return Drive::Reconnect;
    }
    if strategy.on_error(false) {
        return Drive::Rebuild;
    }
    if reconnecting {
        strategy.continue_reconnect();
    } else {
        strategy.arm_init();
    }
    Drive::Rebuild
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterRegistry;
    use std::time::Duration;

    fn resource() -> ResourceEntry {
        ResourceEntry {
            group: String::new(),
            version: "v1".to_string(),
            kind: "Pod".to_string(),
            plural: "pods".to_string(),
            scope: crate::discovery::ResourceScope::Namespaced,
            verbs: vec!["list".to_string()],
            short_names: Vec::new(),
        }
    }

    fn pod(name: &str) -> DynamicObject {
        let value = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "namespace": "default", "uid": format!("uid-{name}") },
        });
        serde_json::from_value(value).expect("valid Pod")
    }

    fn rich_sink() -> (EventSink, mpsc::Receiver<ControllerEvent>) {
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        (EventSink { events }, receiver)
    }

    fn drain<T>(receiver: &mut mpsc::Receiver<T>) -> Vec<T> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    type TestStream =
        futures::stream::Iter<std::vec::IntoIter<watcher::Result<watcher::Event<DynamicObject>>>>;

    fn test_stream(events: Vec<watcher::Result<watcher::Event<DynamicObject>>>) -> TestStream {
        futures::stream::iter(events)
    }

    #[test]
    fn search_timeout_is_finite() {
        assert_eq!(SEARCH_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn search_error_maps_access_failures() {
        let api_error = |code| {
            kube::Error::Api(Box::new(kube::core::Status {
                code,
                ..Default::default()
            }))
        };
        assert!(matches!(
            search_error(api_error(401)),
            SearchError::Unauthorized
        ));
        assert!(matches!(
            search_error(api_error(403)),
            SearchError::Forbidden
        ));
        assert!(matches!(search_error(api_error(500)), SearchError::Api(_)));
        assert_eq!(
            SearchError::NoResources.to_string(),
            "No listable Kubernetes resource types are available. Refresh resource discovery, then retry."
        );
        assert_eq!(
            SearchError::Unavailable.to_string(),
            "Kubernetes resource types were not searched."
        );
    }

    #[test]
    fn search_selector_is_equality_only_for_probable_full_names() {
        assert_eq!(name_field_selector("perf-1-59d"), None);
        assert_eq!(
            name_field_selector(" perf-1-59d59bb66d-2ls9r "),
            Some("metadata.name=perf-1-59d59bb66d-2ls9r".to_owned())
        );
        assert_eq!(name_field_selector("  "), None);
        assert!(!looks_like_complete_resource_name("perf-1-59d"));
        assert!(looks_like_complete_resource_name("perf-1-59d59bb66d-2ls9r"));
    }

    #[test]
    fn search_candidates_rank_exact_prefix_and_contains_and_cap() {
        let candidates = vec![
            pod("my-web-service"),
            pod("web"),
            pod("web-api"),
            pod("w-e-b"),
            pod("other"),
        ];
        let ranked = rank_search_candidates(&resource(), candidates, "web");
        let names: Vec<_> = ranked
            .iter()
            .map(|hit| hit.object.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["web", "web-api", "my-web-service", "w-e-b"]);

        let many: Vec<DynamicObject> = (0..crate::machines::SEARCH_RESULT_LIMIT + 10)
            .map(|index| pod(&format!("web-{index:03}")))
            .collect();
        assert_eq!(
            rank_search_candidates(&resource(), many, "web").len(),
            crate::machines::SEARCH_RESULT_LIMIT
        );
        assert!(rank_search_candidates(&resource(), vec![pod("web")], " ").is_empty());
    }

    #[test]
    fn search_candidates_keep_hyphenated_token_literal() {
        let ranked = rank_search_candidates(
            &resource(),
            vec![pod("network-api"), pod("network-unavailable")],
            "network-unavailable",
        );
        let names: Vec<_> = ranked
            .iter()
            .map(|hit| hit.object.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["network-unavailable"]);
        assert_eq!(
            name_field_selector("network-unavailable"),
            Some("metadata.name=network-unavailable".to_owned())
        );
    }

    #[test]
    fn search_candidates_match_fuzzy_subsequences_case_insensitively() {
        let candidates = vec![pod("w-b-s"), pod("other")];
        let ranked = rank_search_candidates(&resource(), candidates, "WBS");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].object.metadata.name.as_deref(), Some("w-b-s"));

        let candidates = vec![
            pod("my-web-service"),
            pod("web-api"),
            pod("web"),
            pod("other"),
        ];
        let ranked = rank_search_candidates(&resource(), candidates, "WEB");
        let names: Vec<_> = ranked
            .iter()
            .map(|hit| hit.object.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["web", "web-api", "my-web-service"]);
    }

    #[test]
    fn global_hits_rank_names_across_resource_entries() {
        let deployment = ResourceEntry {
            group: "apps".to_owned(),
            version: "v1".to_owned(),
            kind: "Deployment".to_owned(),
            plural: "deployments".to_owned(),
            scope: crate::discovery::ResourceScope::Namespaced,
            verbs: vec!["list".to_owned()],
            short_names: Vec::new(),
        };
        let hits = vec![
            SearchHit {
                resource: deployment,
                object: Arc::new(pod("web-api")),
            },
            SearchHit {
                resource: resource(),
                object: Arc::new(pod("web")),
            },
        ];
        let ranked = rank_search_hits(hits, "web");
        let names: Vec<_> = ranked
            .iter()
            .map(|hit| hit.object.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["web", "web-api"]);
        assert_eq!(ranked[0].resource.kind, "Pod");
        assert_eq!(ranked[1].resource.kind, "Deployment");
    }

    #[tokio::test]
    #[ignore = "Requires kind-k8s-gpui-dev and scripts/seed-pods.sh"]
    async fn search_resources_against_kind_cluster() {
        if !crate::cluster::kubeconfig_present() {
            return;
        }
        let registry = ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == "kind-k8s-gpui-dev")
        else {
            return;
        };
        let api_resource = resource().to_api_resource();
        let api: Api<DynamicObject> = Api::all_with(cluster.client().clone(), &api_resource);
        let all = api.list(&ListParams::default()).await.expect("list Pods");
        let Some(object) = all.items.iter().find(|object| {
            object.metadata.namespace.as_deref() == Some("perf")
                && object
                    .metadata
                    .name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("perf-"))
        }) else {
            return;
        };
        let name = object.metadata.name.clone().expect("Pod has a name");
        let prefix = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cluster.search_resources_with_limit(&resource(), "perf-1-59d"),
        )
        .await
        .expect("prefix search does not time out")
        .expect("prefix search succeeds");
        assert!(prefix.hits.iter().any(|hit| {
            hit.object
                .metadata
                .name
                .as_deref()
                .is_some_and(|value| value.starts_with("perf-1-59d"))
        }));

        let contains = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cluster.search_resources_with_limit(&resource(), "59d59bb66d"),
        )
        .await
        .expect("substring search does not time out")
        .expect("substring search succeeds");
        assert!(contains.hits.iter().any(|hit| {
            hit.object
                .metadata
                .name
                .as_deref()
                .is_some_and(|value| value.contains("59d59bb66d"))
        }));

        let fuzzy = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cluster.search_resources_with_limit(&resource(), "p159"),
        )
        .await
        .expect("subsequence search does not time out")
        .expect("subsequence search succeeds");
        assert!(fuzzy.hits.iter().any(|hit| {
            hit.object
                .metadata
                .name
                .as_deref()
                .is_some_and(|value| fuzzy::score("p159", value).is_some())
        }));

        let exact = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cluster.search_resources_with_limit(&resource(), &name),
        )
        .await
        .expect("full-name search does not time out")
        .expect("full-name search succeeds");
        assert!(!exact.truncated);
        assert!(
            exact
                .hits
                .iter()
                .any(|hit| hit.object.metadata.name.as_deref() == Some(name.as_str()))
        );

        let broad = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cluster.search_resources_with_limit(&resource(), "perf"),
        )
        .await
        .expect("large-list search does not time out")
        .expect("large-list search succeeds");
        assert!(broad.scanned <= SEARCH_LIST_LIMIT);
        assert!(broad.hits.len() <= crate::machines::SEARCH_RESULT_LIMIT);
        if broad.truncated {
            assert_eq!(broad.scanned, SEARCH_LIST_LIMIT);
        }
    }

    #[test]
    fn watcher_config_follows_tier() {
        let local = watcher_config(&watcher::Config::default(), &WatchOptions::default());
        assert_eq!(local.timeout, Some(60));
        assert_eq!(local.initial_list_strategy, InitialListStrategy::ListWatch);
        assert_eq!(local.page_size, Some(WATCH_PAGE_SIZE));
        assert!(local.bookmarks, "bookmarks remain enabled by default");

        let high = watcher_config(
            &watcher::Config::default(),
            &WatchOptions {
                tier: LatencyTier::HighLatency,
                ..WatchOptions::default()
            },
        );
        assert_eq!(high.timeout, Some(300));
        assert_eq!(
            high.initial_list_strategy,
            InitialListStrategy::StreamingList
        );
        assert_eq!(high.page_size, Some(WATCH_PAGE_SIZE));
        assert!(high.bookmarks);
    }

    #[test]
    fn watcher_config_passes_selectors() {
        let options = WatchOptions {
            label_selector: Some("app=web".to_string()),
            field_selector: Some("metadata.name=pod-a".to_string()),
            ..WatchOptions::default()
        };
        let config = watcher_config(&watcher::Config::default(), &options);
        assert_eq!(config.label_selector.as_deref(), Some("app=web"));
        assert_eq!(
            config.field_selector.as_deref(),
            Some("metadata.name=pod-a")
        );
    }

    #[tokio::test]
    async fn rich_sink_emits_init_boundaries_in_order() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        let first = pod("alpha");
        let second = pod("beta");

        let stream = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitApply(first.clone())),
            Ok(watcher::Event::InitApply(second)),
            Ok(watcher::Event::InitDone),
            Ok(watcher::Event::Apply(first.clone())),
            Ok(watcher::Event::Delete(first)),
        ]);
        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Reconnect);

        let events = drain(&mut receiver);
        assert!(
            matches!(events.first(), Some(ControllerEvent::Init)),
            "Init must come first"
        );
        assert!(matches!(
            events.get(1),
            Some(ControllerEvent::InitApply(obj)) if obj.metadata.name.as_deref() == Some("alpha")
        ));
        assert!(matches!(
            events.get(2),
            Some(ControllerEvent::InitApply(obj)) if obj.metadata.name.as_deref() == Some("beta")
        ));
        assert!(matches!(events.get(3), Some(ControllerEvent::InitDone)));
        assert!(matches!(
            events.get(4),
            Some(ControllerEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                ..
            }))
        ));
        assert!(matches!(
            events.get(5),
            Some(ControllerEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                ..
            }))
        ));
        assert_eq!(events.len(), 6);
    }

    #[tokio::test]
    async fn streaming_path_sends_synthetic_init() {
        let (mut sink, mut receiver) = rich_sink();
        assert!(sink.send_init().await);
        assert!(matches!(receiver.recv().await, Some(ControllerEvent::Init)));
    }

    #[test]
    fn streaming_failure_falls_back_once_before_init_done() {
        let mut strategy = ListStrategy::new(LatencyTier::HighLatency);
        assert!(strategy.streaming);
        assert!(
            !strategy.on_error(true),
            "reconnect after InitDone does not fall back"
        );
        assert!(
            strategy.on_error(false),
            "the first initial-sync failure triggers fallback"
        );
        assert!(!strategy.streaming);
        assert!(strategy.fallback_used);
        assert!(!strategy.on_error(false), "fallback happens once");
        assert_eq!(
            strategy
                .config(&watcher::Config::default())
                .initial_list_strategy,
            InitialListStrategy::ListWatch
        );
    }

    #[test]
    fn reconnect_backoff_grows_to_cap_and_resets() {
        let mut backoff = ReconnectBackoff::default();
        assert_eq!(backoff.next(), Duration::from_secs(1));
        assert_eq!(backoff.next(), Duration::from_secs(2));
        assert_eq!(backoff.next(), Duration::from_secs(4));
        assert_eq!(backoff.next(), Duration::from_secs(8));
        assert_eq!(backoff.next(), Duration::from_secs(16));
        assert_eq!(backoff.next(), Duration::from_secs(30));
        assert_eq!(backoff.next(), Duration::from_secs(30));
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn successful_init_resets_reconnect_backoff() {
        let (mut sink, _receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(1));
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(2));
        let stream = test_stream(vec![Ok(watcher::Event::Init), Ok(watcher::Event::InitDone)]);
        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Reconnect);
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(1));
    }

    #[test]
    fn local_tier_never_falls_back() {
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        assert!(!strategy.on_error(false));
        assert!(!strategy.fallback_used);
        assert_eq!(
            strategy
                .config(&watcher::Config::default())
                .initial_list_strategy,
            InitialListStrategy::ListWatch
        );
    }

    #[tokio::test]
    async fn streaming_error_before_init_requests_rebuild() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::HighLatency);
        let stream = test_stream(vec![Err(watcher::Error::NoResourceVersion)]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Rebuild);
        assert!(!strategy.streaming);
        assert!(strategy.init_pending);
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(1));
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(2));
        assert!(drain(&mut receiver).is_empty());
    }

    #[tokio::test]
    async fn failed_list_watch_rearms_init_before_next_authoritative_list() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);

        assert!(strategy.take_init_boundary());
        assert!(sink.send_init().await);
        let first = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitApply(pod("old"))),
            Err(watcher::Error::NoResourceVersion),
        ]);
        assert_eq!(
            drive_stream_with_boundary(first, &mut sink, &mut strategy, true).await,
            Drive::Rebuild
        );
        assert!(strategy.take_init_boundary());
        assert!(sink.send_init().await);

        let second = test_stream(vec![
            Ok(watcher::Event::InitApply(pod("new"))),
            Ok(watcher::Event::InitDone),
        ]);
        assert_eq!(
            drive_stream_with_boundary(second, &mut sink, &mut strategy, true).await,
            Drive::Reconnect
        );

        let events = drain(&mut receiver);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ControllerEvent::Init))
                .count(),
            2
        );
        assert!(matches!(&events[0], ControllerEvent::Init));
        assert!(matches!(&events[1], ControllerEvent::InitApply(_)));
        assert!(matches!(&events[2], ControllerEvent::Init));
        assert!(matches!(&events[3], ControllerEvent::InitApply(_)));
        assert!(matches!(&events[4], ControllerEvent::InitDone));
    }

    #[tokio::test]
    async fn streaming_failure_rearms_init_for_fallback_list() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::HighLatency);

        assert!(strategy.take_init_boundary());
        assert!(sink.send_init().await);
        let first = test_stream(vec![
            Ok(watcher::Event::InitApply(pod("old"))),
            Err(watcher::Error::NoResourceVersion),
        ]);
        assert_eq!(
            drive_stream_with_boundary(first, &mut sink, &mut strategy, true).await,
            Drive::Rebuild
        );
        assert!(!strategy.streaming);
        assert!(strategy.take_init_boundary());
        assert!(sink.send_init().await);

        let second = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitApply(pod("new"))),
            Ok(watcher::Event::InitDone),
        ]);
        assert_eq!(
            drive_stream_with_boundary(second, &mut sink, &mut strategy, true).await,
            Drive::Reconnect
        );

        let events = drain(&mut receiver);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ControllerEvent::Init))
                .count(),
            2
        );
        assert!(matches!(&events[0], ControllerEvent::Init));
        assert!(matches!(&events[1], ControllerEvent::InitApply(_)));
        assert!(matches!(&events[2], ControllerEvent::Init));
        assert!(matches!(&events[3], ControllerEvent::InitApply(_)));
        assert!(matches!(&events[4], ControllerEvent::InitDone));
    }

    #[tokio::test]
    async fn synthetic_init_does_not_duplicate_watcher_init() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::HighLatency);
        let stream = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitApply(pod("alpha"))),
            Ok(watcher::Event::InitDone),
        ]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, true).await;
        assert_eq!(drive, Drive::Reconnect);
        let events = drain(&mut receiver);
        assert!(matches!(
            events.first(),
            Some(ControllerEvent::InitApply(_))
        ));
        assert!(matches!(events.get(1), Some(ControllerEvent::InitDone)));
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn initial_error_keeps_current_sync_boundary() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        let stream = test_stream(vec![Err(watcher::Error::NoResourceVersion)]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Rebuild);
        assert!(drain(&mut receiver).is_empty());
        assert!(strategy.init_pending);
    }

    #[tokio::test]
    async fn post_init_error_arms_one_reconnect_boundary() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        let stream = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitDone),
            Err(watcher::Error::NoResourceVersion),
        ]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Reconnect);
        let events = drain(&mut receiver);
        assert!(matches!(events.first(), Some(ControllerEvent::Init)));
        assert!(matches!(events.get(1), Some(ControllerEvent::InitDone)));
        assert_eq!(events.len(), 2);
        assert!(strategy.init_pending);

        assert!(strategy.take_init_boundary());
        assert!(sink.send_init().await);
        let retry = test_stream(vec![
            Ok(watcher::Event::Init),
            Ok(watcher::Event::InitApply(pod("new"))),
            Ok(watcher::Event::InitDone),
        ]);
        assert_eq!(
            drive_stream_with_boundary(retry, &mut sink, &mut strategy, true).await,
            Drive::Reconnect
        );
        let events = drain(&mut receiver);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ControllerEvent::Init))
                .count(),
            1
        );
        assert!(matches!(&events[0], ControllerEvent::Init));
        assert!(matches!(&events[1], ControllerEvent::InitApply(_)));
        assert!(matches!(&events[2], ControllerEvent::InitDone));
    }

    #[tokio::test]
    async fn post_init_stream_end_waits_before_retry_init() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        let stream = test_stream(vec![Ok(watcher::Event::Init), Ok(watcher::Event::InitDone)]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Reconnect);
        let events = drain(&mut receiver);
        assert!(matches!(events.first(), Some(ControllerEvent::Init)));
        assert!(matches!(events.get(1), Some(ControllerEvent::InitDone)));
        assert_eq!(events.len(), 2);
        assert!(strategy.init_pending);

        let retry = async {
            wait_for_reconnect(&mut strategy).await;
            assert!(strategy.take_init_boundary());
            sink.send_init().await
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), retry)
                .await
                .is_err()
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropped_receiver_stops_watch() {
        let (mut sink, receiver) = rich_sink();
        drop(receiver);
        let mut strategy = ListStrategy::new(LatencyTier::Local);
        let stream = test_stream(vec![Ok(watcher::Event::Apply(pod("alpha")))]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Closed);
    }

    #[tokio::test]
    #[ignore = "Requires a kind cluster: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn init_events_against_kind_cluster() {
        if !crate::cluster::kubeconfig_present() {
            return;
        }
        let registry = ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let (controller, mut events) = Controller::spawn_with_events(
            cluster.client().clone(),
            resource().to_api_resource(),
            Scope::All,
            WatchOptions {
                tier: LatencyTier::HighLatency,
                ..WatchOptions::default()
            },
        )
        .expect("runtime is available");

        let observed = tokio::time::timeout(Duration::from_secs(30), async {
            let mut saw_init = false;
            let mut init_applied = 0_usize;
            while let Some(event) = events.recv().await {
                match event {
                    ControllerEvent::Init => saw_init = true,
                    ControllerEvent::InitApply(_) => {
                        assert!(saw_init, "InitApply must follow Init");
                        init_applied += 1;
                    }
                    ControllerEvent::InitDone => break,
                    ControllerEvent::Store(_) => {}
                }
            }
            (saw_init, init_applied)
        })
        .await
        .expect("initial sync completes within 30 seconds");

        assert!(observed.0, "Init was received");
        assert!(observed.1 > 0, "the kind kube-system namespace has Pods");
        drop(controller);
    }
}
