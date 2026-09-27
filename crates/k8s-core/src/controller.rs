//! Runs one watcher and reflector for each cluster, GVR, and namespace.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::Client;
use kube::api::{Api, ApiResource, ListParams};
use kube::core::{DynamicObject, Status};
use kube::runtime::watcher;
use kube::runtime::watcher::InitialListStrategy;
use tokio::sync::{mpsc, watch};
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

/// Why a watch failed, in the terms a reader can act on.
///
/// `Display` writes the lower-case word so it can be logged as a field. The
/// variants answer the only question that decides what happens next: did the
/// cluster *answer* (`Forbidden`, `Unauthorized`, `NotFound`) or did we never
/// find out (`Throttled`, `Unavailable`, `Transport`, `Stream`, `Unknown`)? An
/// answer is only changed by something outside this process, which is why it gets
/// a much shorter retry budget — see [`REFUSAL_RETRY_LIMIT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// 403. The identity asked is not allowed to do this.
    Forbidden,
    /// 401. The credentials presented were refused.
    Unauthorized,
    /// 404. The cluster does not serve this resource.
    NotFound,
    /// 429. The server asked us to slow down.
    Throttled,
    /// 5xx. The cluster answered, and the answer was a failure.
    Unavailable,
    /// The connection failed. Nothing is known about the cluster's answer.
    ///
    /// A request that ran out of time is one of these: `kube` reports it as a
    /// `HyperError`, and no reachable API says whether it was the deadline or the
    /// socket. Every variant here is one something actually produces, because a
    /// kind nothing can produce is a kind a reader has to guess about.
    Transport,
    /// The watch ended, or could not be resumed where it left off.
    Stream,
    /// Anything else.
    Unknown,
}

impl FailureKind {
    /// True when the cluster answered, so the answer will not change by asking
    /// the same question again.
    fn is_answer(self) -> bool {
        matches!(self, Self::Forbidden | Self::Unauthorized | Self::NotFound)
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Forbidden => "forbidden",
            Self::Unauthorized => "unauthorized",
            Self::NotFound => "not-found",
            Self::Throttled => "throttled",
            Self::Unavailable => "unavailable",
            Self::Transport => "transport",
            Self::Stream => "stream",
            Self::Unknown => "unknown",
        })
    }
}

/// One failed watch attempt, described well enough to act on.
///
/// `UI-SPEC.md` §4.15 asks a permission failure to say what the identity can do,
/// what it cannot, and why — three questions, and only the third is the API
/// server's to answer. So the server's own words are kept verbatim in
/// [`Self::message`], and everything a reader could act on without them is broken
/// out into fields: `verb` and `resource` name the request that was refused,
/// `group` and `namespace` say where, `identity` says who was refused. §4.15's
/// remaining question — what the identity *can* do — has no field here, because
/// only a `can-i` probe can answer it and this struct is not a place to make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchFailure {
    /// What sort of failure this is.
    pub kind: FailureKind,
    /// The API server's message, verbatim. Falls back to the watcher's own words
    /// when the failure happened before the server answered, and is never empty.
    pub message: String,
    /// The verb the watch needs. Defaults to `list` until the initial list
    /// completes and `watch` after it, because that is the request the watch
    /// makes at that point; a denial message overrides the guess.
    pub verb: Option<String>,
    /// The resource the watch asked for.
    pub resource: Option<String>,
    /// The API group. `Some("")` is the core group.
    pub group: Option<String>,
    /// The namespace the request was scoped to, when the refusal named one.
    pub namespace: Option<String>,
    /// The identity the API server refused, without its `User`/`Group` prefix.
    pub identity: Option<String>,
    /// Consecutive failures of this kind, counting this one. `1` on the first.
    pub attempts: u32,
    /// False once the retry budget is spent. A failure that is not retrying is
    /// final for this subscription: the watch has stopped, and starting a new one
    /// is the only way to try again — which is what lets a permission granted
    /// mid-session take effect after a refresh instead of never.
    pub retrying: bool,
}

impl WatchFailure {
    /// The request as a phrase, for `list pods in the prod namespace`.
    fn target(&self) -> String {
        let mut target = match (&self.verb, &self.resource) {
            (Some(verb), Some(resource)) => format!("{verb} {resource}"),
            (Some(verb), None) => verb.clone(),
            (None, Some(resource)) => resource.clone(),
            (None, None) => return "the Kubernetes request".to_owned(),
        };
        if let Some(namespace) = &self.namespace {
            target.push_str(&format!(" in the {namespace} namespace"));
        }
        target
    }
}

impl fmt::Display for WatchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = self.target();
        match self.kind {
            FailureKind::Forbidden => write!(f, "Cannot {target}.")?,
            FailureKind::Unauthorized => {
                write!(f, "The cluster refused the identity for {target}.")?
            }
            FailureKind::NotFound => write!(
                f,
                "This cluster does not serve {}.",
                self.resource.as_deref().unwrap_or("this resource")
            )?,
            FailureKind::Throttled => write!(f, "The cluster is throttling {target}.")?,
            FailureKind::Unavailable => write!(f, "The cluster could not answer for {target}.")?,
            FailureKind::Transport => write!(f, "The connection failed while running {target}.")?,
            FailureKind::Stream => write!(f, "The watch for {target} ended.")?,
            FailureKind::Unknown => write!(f, "{target} failed.")?,
        }
        write!(f, " {}", self.message)?;
        match self.kind {
            FailureKind::Forbidden | FailureKind::NotFound => {
                f.write_str(" Grant the missing permission or switch context, then Retry.")
            }
            FailureKind::Unauthorized => {
                f.write_str(" Refresh the cluster credentials, then Retry.")
            }
            _ => f.write_str(" Check the cluster connection, then Retry."),
        }
    }
}

/// The current failure state of one watch, shareable with a reader.
///
/// A failed watch says so here instead of only in the log: `UI-SPEC.md` §4.15
/// wants a failure stated where it happens, and the log is nowhere a reader can
/// see. [`Controller::status`] hands this out, and it is a watch channel rather
/// than an event stream for one reason — a reader that arrives late must still be
/// able to ask what happened, and must not be able to miss a failure that landed
/// before it asked. A receiver from [`Self::subscribe`] holds the current value
/// already, so there is no window between asking and listening.
#[derive(Clone, Debug)]
pub struct WatchStatus {
    failure: watch::Sender<Option<Arc<WatchFailure>>>,
}

impl WatchStatus {
    fn new() -> Self {
        let (failure, _) = watch::channel(None);
        Self { failure }
    }

    /// Watches the failure change. The returned receiver already holds the
    /// current value, so `borrow()` is the failure as of right now and `changed()`
    /// waits for the next one.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<WatchFailure>>> {
        self.failure.subscribe()
    }

    /// The failure this watch is in right now, if any.
    pub fn failure(&self) -> Option<Arc<WatchFailure>> {
        (*self.failure.borrow()).clone()
    }

    fn report(&self, failure: Arc<WatchFailure>) {
        self.failure.send_replace(Some(failure));
    }

    fn clear(&self) {
        self.failure.send_replace(None);
    }
}

/// Attempts before a refusal stops the watch for good.
///
/// A refusal is the API server's *answer*, not an outage: asking again cannot
/// change it, and the only thing that can is somebody editing RBAC, which happens
/// outside this process. Three attempts is the smallest budget that still rides
/// out the two refusals that are not really refusals — an exec credential plugin
/// answering 401 while it refreshes a token, and an aggregated API server
/// answering 403 while its backend pod restarts — and it costs three seconds of
/// backoff, well inside the ten seconds `PROMPT.md` §2.4 allows a network problem
/// to take to become visible. Making it larger would delay the one thing that
/// helps a refused reader, which is being told.
pub const REFUSAL_RETRY_LIMIT: u32 = 3;

/// Attempts before a failure with no answer in it stops the watch for good.
///
/// Nothing was learned from these, so the budget only has to be long enough to
/// ride out a real outage: 1s + 2s + 4s + 8s + 16s + 30s of backoff is about a
/// minute, twice the initial-sync budget the UI already gives a first load. After
/// that the watch stops and says why, rather than looping quietly until the
/// window closes.
pub const TRANSPORT_RETRY_LIMIT: u32 = 6;

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
    /// Where this watch publishes its failures. Shared with the watch task, so
    /// cloning it is the whole of the reader-side contract.
    status: WatchStatus,
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
        let status = WatchStatus::new();
        let watch_task = runtime.spawn(watch_loop(
            api,
            config,
            resource,
            options.tier,
            EventSink { events },
            status.clone(),
        ));
        Ok((
            Self {
                tier: options.tier,
                status,
                watch_task,
            },
            receiver,
        ))
    }

    pub fn tier(&self) -> LatencyTier {
        self.tier
    }

    /// The watch's failure state, for a reader that has to say what went wrong.
    ///
    /// This is deliberately separate from the event stream: an event that a
    /// reader has already consumed cannot be asked about again, and the one
    /// failure worth reporting is usually the first one — the initial list that
    /// was refused, before there was anything to show.
    pub fn status(&self) -> WatchStatus {
        self.status.clone()
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

/// One failure, read off the error the watcher returned.
#[derive(Debug)]
struct ClassifiedFailure {
    kind: FailureKind,
    /// The HTTP status, when the API server answered.
    code: Option<u16>,
    message: String,
}

impl ClassifiedFailure {
    /// A watch stream that ended without an error of its own. A watch that a
    /// server closed is still something a reader should hear about, so it is
    /// accounted for like any other failure.
    fn stream_ended() -> Self {
        Self {
            kind: FailureKind::Stream,
            code: None,
            message: "The watch stream ended before the cluster closed it.".to_owned(),
        }
    }
}

/// Splits one watcher error into the part a reader acts on and the rest.
///
/// The initial list is the failure that matters most and it arrives as
/// `InitialListFailed`, so it is read first here: a refusal to list is the same
/// refusal whether the watcher was about to list or to watch.
fn classify_failure(error: &watcher::Error) -> ClassifiedFailure {
    let mut failure = match error {
        watcher::Error::InitialListFailed(source)
        | watcher::Error::WatchStartFailed(source)
        | watcher::Error::WatchFailed(source) => classify_client_error(source),
        watcher::Error::WatchError(status) => classify_status(status),
        watcher::Error::NoResourceVersion => ClassifiedFailure {
            kind: FailureKind::Stream,
            code: None,
            message: String::new(),
        },
    };
    // A proxy or a webhook can answer with a status and no words, and a reason
    // with no words in it is not a reason a reader can use.
    if failure.message.is_empty() {
        failure.message = error.to_string();
    }
    failure
}

/// An API status when the server answered, a dead connection when it did not.
fn classify_client_error(error: &kube::Error) -> ClassifiedFailure {
    match error {
        kube::Error::Api(status) => classify_status(status),
        other => ClassifiedFailure {
            // Nothing was learned, so this is not the cluster's answer and must
            // not be reported as one.
            kind: FailureKind::Transport,
            code: None,
            message: other.to_string(),
        },
    }
}

fn classify_status(status: &Status) -> ClassifiedFailure {
    let kind = match status.code {
        401 => FailureKind::Unauthorized,
        403 => FailureKind::Forbidden,
        404 => FailureKind::NotFound,
        // Too old to resume from. The watch restarts from a new list, so this is
        // the watch losing its place rather than the cluster refusing it.
        410 => FailureKind::Stream,
        429 => FailureKind::Throttled,
        500..=599 => FailureKind::Unavailable,
        _ => FailureKind::Unknown,
    };
    ClassifiedFailure {
        kind,
        code: Some(status.code),
        message: status.message.clone(),
    }
}

/// What an authorization refusal says about the request it refused.
///
/// The API server writes these in exactly one place, so the shape is stable:
/// `<resource> is forbidden: <identity> cannot <verb> resource "<resource>" in API
/// group "<group>" (in the namespace "<namespace>"|at the cluster scope)`. Every
/// field is filled independently and a mismatch stops the parse rather than the
/// failure: the message is already plain words, and a reader is better served by
/// words this parser did not recognise than by a half-filled struct that reads as
/// a complete one.
#[derive(Debug, Default, PartialEq, Eq)]
struct Denial {
    verb: Option<String>,
    resource: Option<String>,
    group: Option<String>,
    namespace: Option<String>,
    identity: Option<String>,
}

impl Denial {
    fn parse(message: &str) -> Self {
        let mut denial = Self::default();
        let Some((identity, rest)) = message.split_once(" cannot ") else {
            return denial;
        };
        denial.identity = last_quoted_value(identity).map(str::to_owned);
        let Some((verb, rest)) = rest.split_once(' ') else {
            return denial;
        };
        if verb.is_empty() {
            return Self::default();
        }
        denial.verb = Some(verb.to_owned());

        // `cannot list resource "pods" in …`, or `cannot list pods in …`, which is
        // what older API servers wrote and what a proxy may still be rewriting to.
        let rest = match rest.trim_start().strip_prefix("resource ") {
            Some(quoted) => match take_quoted(quoted) {
                Some((resource, rest)) => {
                    denial.resource = Some(resource.to_owned());
                    rest
                }
                None => return denial,
            },
            None => match rest.trim_start().split_once(' ') {
                Some((resource, rest)) => {
                    denial.resource = Some(resource.to_owned());
                    rest
                }
                None => {
                    denial.resource = Some(rest.trim().to_owned());
                    return denial;
                }
            },
        };

        if let Some(after_group) = rest.trim_start().strip_prefix("in API group ")
            && let Some((group, rest)) = take_quoted(after_group)
        {
            denial.group = Some(group.to_owned());
            // `at the cluster scope` and the caller's groups are both left
            // alone: the first means there is no namespace, and the second is
            // about the identity rather than the request.
            if let Some(after_namespace) = rest.trim_start().strip_prefix("in the namespace ")
                && let Some((namespace, _)) = take_quoted(after_namespace)
            {
                denial.namespace = Some(namespace.to_owned());
            }
        }
        denial
    }
}

/// Splits `"value"` off the front of the text, returning it and whatever follows
/// the closing quote.
fn take_quoted(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let rest = text.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some((&rest[..end], &rest[end + 1..]))
}

/// The last `"quoted"` value in the text, which is how the API server names the
/// identity it refused. Walking the quotes in pairs keeps a name that contains a
/// space — a certificate subject, most often — intact.
fn last_quoted_value(text: &str) -> Option<&str> {
    let mut quotes = text.match_indices('"');
    let mut value = None;
    while let (Some((open, _)), Some((close, _))) = (quotes.next(), quotes.next()) {
        value = Some(&text[open + 1..close]);
    }
    value
}

/// Counts consecutive failures of one kind, so the watch can stop retrying.
///
/// The key is the kind and the status code, deliberately not the message: a
/// message that varies — a new socket, a different proxy — is the same problem,
/// and keying on it would let a flapping connection reset the count on every
/// swing and never trip.
#[derive(Debug, Default)]
struct FailureLedger {
    key: Option<(FailureKind, Option<u16>)>,
    attempts: u32,
}

impl FailureLedger {
    /// Charges one failure and answers whether to try again.
    fn record(&mut self, key: (FailureKind, Option<u16>)) -> (u32, bool) {
        if self.key != Some(key) {
            // A different problem is a different budget. Charging a new failure
            // against the last one's budget is how a 403 followed by a 503 turns
            // into one long outage verdict.
            self.key = Some(key);
            self.attempts = 0;
        }
        self.attempts = self.attempts.saturating_add(1);
        (self.attempts, self.attempts < retry_limit(key.0))
    }

    /// Forgets the last failure entirely.
    fn reset(&mut self) {
        self.key = None;
        self.attempts = 0;
    }
}

fn retry_limit(kind: FailureKind) -> u32 {
    if kind.is_answer() {
        REFUSAL_RETRY_LIMIT
    } else {
        TRANSPORT_RETRY_LIMIT
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
    /// What the watch asked for, so a failure can name the request even when the
    /// message does not.
    request: Option<ApiResource>,
    /// Consecutive failures of the same kind, and how much budget is left.
    failures: FailureLedger,
    /// Where the current failure is published.
    status: WatchStatus,
}

impl ListStrategy {
    /// The strategy for a watch that knows what it asked for and where to publish
    /// its failures.
    fn for_request(tier: LatencyTier, request: Option<ApiResource>, status: WatchStatus) -> Self {
        Self {
            streaming: tier.uses_streaming_lists(),
            fallback_used: false,
            init_pending: true,
            reconnecting: false,
            backoff: ReconnectBackoff::default(),
            request,
            failures: FailureLedger::default(),
            status,
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
    ///
    /// This is a change of mechanism, not a recovery, so it does not undo the
    /// failure the caller already recorded: a list that the streaming watch
    /// refused is still a list the fallback watch has to make, and two refusals
    /// from two mechanisms are two data points rather than one.
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
        // A completed authoritative list answers every failure before it, so the
        // budget starts over and the reader's error goes away. Forgetting it here
        // is what keeps a session that fails once an hour for a week from
        // eventually deciding it is out of retries.
        self.failures.reset();
        self.status.clear();
    }

    /// Charges one failure, publishes it, and answers whether to try again.
    ///
    /// The verb and the resource fall back to what this watch asked for when the
    /// message does not say. The controller knows it went to `list pods`, and a
    /// reader told "cannot list pods: connection refused" has learned something
    /// they can act on, while a reader told only "connection refused" has not
    /// learned which request to go and fix.
    fn record_failure(&mut self, failure: ClassifiedFailure, init_done: bool) -> Arc<WatchFailure> {
        let denial = Denial::parse(&failure.message);
        let (attempts, retrying) = self.failures.record((failure.kind, failure.code));
        let request = self.request.as_ref();
        let published = Arc::new(WatchFailure {
            kind: failure.kind,
            message: failure.message,
            verb: denial
                .verb
                .or_else(|| Some(if init_done { "watch" } else { "list" }.to_owned())),
            resource: denial
                .resource
                .or_else(|| request.map(|request| request.plural.clone())),
            group: denial
                .group
                .or_else(|| request.map(|request| request.group.clone())),
            namespace: denial.namespace,
            identity: denial.identity,
            attempts,
            retrying,
        });
        self.status.report(Arc::clone(&published));
        published
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
        // The counter holds retries already handed out, so the first call is
        // failure one and the shared ladder is asked about that.
        let delay = crate::latency::backoff(u32::from(self.failures) + 1);
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
    request: ApiResource,
    tier: LatencyTier,
    mut sink: EventSink,
    status: WatchStatus,
) {
    let mut strategy = ListStrategy::for_request(tier, Some(request), status);
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
                let failure = strategy.record_failure(classify_failure(&error), init_done);
                tracing::warn!(
                    kind = %failure.kind,
                    attempts = failure.attempts,
                    retrying = failure.retrying,
                    "watch attempt failed: %failure"
                );
                if !failure.retrying {
                    // The budget is spent, so the loop stops. A session that is
                    // not allowed to list must not spend the rest of its life
                    // rebuilding the same refused list, and `WatchStatus` keeps
                    // the reason for a reader that arrives afterwards. Starting a
                    // new subscription is how it tries again, which is also how a
                    // permission granted in the meantime takes effect.
                    tracing::error!(
                        kind = %failure.kind,
                        attempts = failure.attempts,
                        "watch stopped after repeated failures: %failure"
                    );
                    return Drive::Closed;
                }
                if strategy.on_error(init_done) {
                    return Drive::Rebuild;
                }
                if init_done {
                    strategy.start_reconnect();
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
    let failure = strategy.record_failure(ClassifiedFailure::stream_ended(), init_done);
    tracing::warn!(
        kind = %failure.kind,
        attempts = failure.attempts,
        retrying = failure.retrying,
        "watch stream ended: %failure"
    );
    if !failure.retrying {
        return Drive::Closed;
    }
    if init_done {
        strategy.start_reconnect();
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

    /// A strategy for a watch with nothing known about the request, which is all
    /// the init-boundary tests below need.
    fn strategy(tier: LatencyTier) -> ListStrategy {
        ListStrategy::for_request(tier, None, WatchStatus::new())
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

    #[tokio::test]
    async fn rich_sink_emits_init_boundaries_in_order() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = strategy(LatencyTier::Local);
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

    #[tokio::test]
    async fn successful_init_resets_reconnect_backoff() {
        let (mut sink, _receiver) = rich_sink();
        let mut strategy = strategy(LatencyTier::Local);
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(1));
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(2));
        let stream = test_stream(vec![Ok(watcher::Event::Init), Ok(watcher::Event::InitDone)]);
        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Reconnect);
        assert_eq!(strategy.reconnect_delay(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn streaming_error_before_init_requests_rebuild() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = strategy(LatencyTier::HighLatency);
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
        let mut strategy = strategy(LatencyTier::Local);

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
        let mut strategy = strategy(LatencyTier::HighLatency);

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
        let mut strategy = strategy(LatencyTier::HighLatency);
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
        let mut strategy = strategy(LatencyTier::Local);
        let stream = test_stream(vec![Err(watcher::Error::NoResourceVersion)]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Rebuild);
        assert!(drain(&mut receiver).is_empty());
        assert!(strategy.init_pending);
    }

    #[tokio::test]
    async fn post_init_error_arms_one_reconnect_boundary() {
        let (mut sink, mut receiver) = rich_sink();
        let mut strategy = strategy(LatencyTier::Local);
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
        let mut strategy = strategy(LatencyTier::Local);
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
        let mut strategy = strategy(LatencyTier::Local);
        let stream = test_stream(vec![Ok(watcher::Event::Apply(pod("alpha")))]);

        let drive = drive_stream_with_boundary(stream, &mut sink, &mut strategy, false).await;
        assert_eq!(drive, Drive::Closed);
    }

    /// The refusal the API server sends for a Pod list the identity may not make.
    const REFUSAL: &str = "pods is forbidden: User \"dev\" cannot list resource \"pods\" in API \
                          group \"\" in the namespace \"prod\" in the group [\"system:authenticated\"]";

    /// A refusal of the initial list, which is how a watch with no permission
    /// starts.
    fn refused_list(message: &str) -> watcher::Error {
        watcher::Error::InitialListFailed(kube::Error::Api(Box::new(Status {
            code: 403,
            reason: "Forbidden".to_owned(),
            message: message.to_owned(),
            ..Default::default()
        })))
    }

    fn pod_strategy(status: WatchStatus) -> ListStrategy {
        ListStrategy::for_request(
            LatencyTier::Local,
            Some(resource().to_api_resource()),
            status,
        )
    }

    #[test]
    fn a_refusal_names_the_request_it_refused() {
        let parsed = Denial::parse(REFUSAL);
        assert_eq!(parsed.verb.as_deref(), Some("list"));
        assert_eq!(parsed.resource.as_deref(), Some("pods"));
        assert_eq!(parsed.group.as_deref(), Some(""));
        assert_eq!(parsed.namespace.as_deref(), Some("prod"));
        assert_eq!(parsed.identity.as_deref(), Some("dev"));

        // Older API servers wrote `cannot list pods`, with no `resource` word.
        let older = Denial::parse(
            "nodes is forbidden: User \"dev\" cannot list nodes in API group \"\" at the cluster scope",
        );
        assert_eq!(older.verb.as_deref(), Some("list"));
        assert_eq!(older.resource.as_deref(), Some("nodes"));
        assert_eq!(older.namespace, None);

        // A refusal this parser does not recognise invents nothing: the words are
        // still reported, and only the fields stay empty.
        assert_eq!(
            Denial::parse("the server is currently unable to handle the request"),
            Denial::default()
        );
    }

    #[tokio::test]
    async fn a_refused_list_is_named_and_then_the_watch_stops() {
        let status = WatchStatus::new();
        let mut reader = status.subscribe();
        let (mut sink, mut events) = rich_sink();
        let mut strategy = pod_strategy(status);

        assert!(reader.borrow().is_none());
        for attempt in 1..=REFUSAL_RETRY_LIMIT {
            let drive = drive_stream_with_boundary(
                test_stream(vec![Err(refused_list(REFUSAL))]),
                &mut sink,
                &mut strategy,
                false,
            )
            .await;
            // Bounded, so a watch that stopped publishing fails here instead of
            // waiting for a change that will never come.
            tokio::time::timeout(Duration::from_secs(1), reader.changed())
                .await
                .expect("the failure is published")
                .expect("the status outlives the watch");
            let reported = (*reader.borrow_and_update()).clone().expect("a failure");
            assert_eq!(reported.kind, FailureKind::Forbidden);
            assert_eq!(reported.message, REFUSAL, "the body is passed on verbatim");
            assert_eq!(reported.verb.as_deref(), Some("list"));
            assert_eq!(reported.resource.as_deref(), Some("pods"));
            assert_eq!(reported.group.as_deref(), Some(""));
            assert_eq!(reported.namespace.as_deref(), Some("prod"));
            assert_eq!(reported.identity.as_deref(), Some("dev"));
            assert_eq!(reported.attempts, attempt);
            assert_eq!(reported.retrying, attempt < REFUSAL_RETRY_LIMIT);
            assert_eq!(
                drive,
                if attempt < REFUSAL_RETRY_LIMIT {
                    Drive::Rebuild
                } else {
                    Drive::Closed
                },
                "the budget runs out on attempt {attempt}"
            );
        }
        assert!(drain(&mut events).is_empty());
    }

    #[tokio::test]
    async fn the_budget_restarts_after_a_completed_list_or_a_different_failure() {
        let status = WatchStatus::new();
        let (mut sink, _events) = rich_sink();
        let mut strategy = pod_strategy(status.clone());
        let refusal = || test_stream(vec![Err(refused_list(REFUSAL))]);
        let attempts =
            |status: &WatchStatus| status.failure().expect("a failure is published").attempts;

        drive_stream_with_boundary(refusal(), &mut sink, &mut strategy, false).await;
        drive_stream_with_boundary(refusal(), &mut sink, &mut strategy, false).await;
        // A different kind is a different problem and is charged from one.
        drive_stream_with_boundary(
            test_stream(vec![Err(watcher::Error::NoResourceVersion)]),
            &mut sink,
            &mut strategy,
            false,
        )
        .await;
        assert_eq!(attempts(&status), 1);

        // A completed list answers every failure before it, so the reader's error
        // goes away.
        strategy.on_init_done();
        assert!(status.failure().is_none());
        drive_stream_with_boundary(refusal(), &mut sink, &mut strategy, false).await;
        assert_eq!(
            attempts(&status),
            1,
            "a session that fails once an hour must not run out of retries"
        );
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
