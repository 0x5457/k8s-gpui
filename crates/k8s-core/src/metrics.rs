//! Fetches metrics-server samples and normalizes time series.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

use crate::latency::{LatencyTier, NON_WATCH_READ_TIMEOUT, with_read_timeout};

/// Root path for the metrics-server API.
pub(crate) const METRICS_API_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

/// Default sample interval for the local tier.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// Sample interval for the high-latency tier.
pub(crate) const HIGH_LATENCY_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum normalized slots. The newest range is kept when the limit is exceeded.
pub(crate) const MAX_NORMALIZED_SLOTS: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// A 404 response means that metrics-server is not installed.
    #[error(
        "metrics-server is not installed in the cluster. Install metrics-server and try again."
    )]
    Unavailable,

    /// A 403 response means the credentials lack a permission.
    ///
    /// The cluster answered, so this is not a connection problem and the copy
    /// names the permission instead of the connection. `detail` keeps the status
    /// code and the API server's own text for logs and `kubectl` output.
    #[error(
        "The cluster denied access to {path}: {detail}. Grant the {permission} permission, then try again."
    )]
    Forbidden {
        path: String,
        /// The RBAC rule the request needs.
        permission: String,
        /// The API server's message for this denial.
        detail: String,
    },

    #[error(
        "Failed to build the metrics request for {path}: {source}. Check the namespace and resource path."
    )]
    Request {
        path: String,
        #[source]
        source: http::Error,
    },

    #[error("metrics-server request failed: {source}. Check the cluster connection and try again.")]
    Api {
        #[source]
        source: Box<kube::Error>,
    },

    #[error(
        "The metrics-server request timed out after 30 seconds. Check the cluster connection and try again."
    )]
    Timeout,

    #[error(
        "The metrics response contains a missing or invalid resource quantity. Check metrics-server and try again."
    )]
    Quantity { value: String },
}

impl MetricsError {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }

    /// True when the cluster refused the request instead of failing to answer.
    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Forbidden { .. })
    }

    /// The RBAC permission a denied request needs, such as `list nodes.metrics.k8s.io`.
    pub fn permission(&self) -> Option<&str> {
        match self {
            Self::Forbidden { permission, .. } => Some(permission),
            _ => None,
        }
    }
}

/// The RBAC permission a metrics path needs.
///
/// The resource is written the way a `Role` names it, group included, so the
/// copy can be pasted into a role or a `kubectl auth can-i` command. A
/// single-object path names the collection, because the permission covers the
/// whole collection either way.
pub fn metrics_permission(path: &str) -> String {
    let rest = path
        .strip_prefix(METRICS_API_PATH)
        .unwrap_or(path)
        .trim_matches('/');
    let (namespace, resource) = match rest.strip_prefix("namespaces/") {
        Some(tail) => {
            let (namespace, tail) = tail.split_once('/').unwrap_or((tail, ""));
            (Some(namespace), first_segment(tail))
        }
        None => (None, first_segment(rest)),
    };
    match (resource, namespace) {
        ("", _) => "get the metrics.k8s.io API discovery endpoint".to_owned(),
        (resource, Some(namespace)) => {
            format!("list {resource}.metrics.k8s.io in the {namespace} namespace")
        }
        (resource, None) => format!("list {resource}.metrics.k8s.io"),
    }
}

fn first_segment(path: &str) -> &str {
    path.split('/').next().unwrap_or_default()
}

/// Parse a Kubernetes quantity as a floating-point number.
pub fn parse_quantity(value: &str) -> Result<f64, MetricsError> {
    const BINARY_SUFFIXES: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Pi", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Ei", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ];
    const DECIMAL_SUFFIXES: &[(&str, f64)] = &[
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];

    let trimmed = value.trim();
    let invalid = || MetricsError::Quantity {
        value: value.to_owned(),
    };
    if trimmed.is_empty() {
        return Err(invalid());
    }
    for (suffix, multiplier) in BINARY_SUFFIXES.iter().chain(DECIMAL_SUFFIXES) {
        if let Some(number) = trimmed.strip_suffix(suffix) {
            let parsed: f64 = number.parse().map_err(|_| invalid())?;
            return Ok(parsed * multiplier);
        }
    }
    trimmed.parse().map_err(|_| invalid())
}

/// Convert an RFC3339 timestamp to Unix milliseconds.
pub fn parse_timestamp_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// One node sample.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeMetric {
    pub name: String,
    /// RFC3339 timestamp for display.
    pub timestamp: String,
    /// metrics-server window text, such as `10s`.
    pub window: String,
    pub cpu_millicores: Option<f64>,
    pub memory_bytes: Option<f64>,
}

impl NodeMetric {
    pub fn at_ms(&self) -> Option<i64> {
        parse_timestamp_ms(&self.timestamp)
    }
}

/// One Pod sample. Pod totals are the sum of container values.
#[derive(Clone, Debug, PartialEq)]
pub struct PodMetric {
    pub namespace: String,
    pub name: String,
    pub timestamp: String,
    pub window: String,
    pub containers: Vec<ContainerMetric>,
}

impl PodMetric {
    pub fn at_ms(&self) -> Option<i64> {
        parse_timestamp_ms(&self.timestamp)
    }

    /// Sum values when at least one container has a value.
    pub fn cpu_millicores(&self) -> Option<f64> {
        sum_values(
            self.containers
                .iter()
                .map(|container| container.cpu_millicores),
        )
    }

    pub fn memory_bytes(&self) -> Option<f64> {
        sum_values(
            self.containers
                .iter()
                .map(|container| container.memory_bytes),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerMetric {
    pub name: String,
    pub cpu_millicores: Option<f64>,
    pub memory_bytes: Option<f64>,
}

/// Check whether the metrics API is available. A 404 maps to [`MetricsError::Unavailable`].
pub async fn probe(client: &kube::Client) -> Result<(), MetricsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let request = get(METRICS_API_PATH)?;
            client
                .request_text(request)
                .await
                .map(|_| ())
                .map_err(|error| classify_at(METRICS_API_PATH, error))
        },
        MetricsError::Timeout,
    )
    .await
}

async fn fetch_node_items(client: &kube::Client) -> Result<Vec<NodeMetricsItem>, MetricsError> {
    let path = format!("{METRICS_API_PATH}/nodes");
    let request = get(&path)?;
    let list: NodeMetricsList = client
        .request(request)
        .await
        .map_err(|error| classify_at(&path, error))?;
    Ok(list.items)
}

/// Fetch samples for all nodes.
pub async fn fetch_nodes(client: &kube::Client) -> Result<Vec<NodeMetric>, MetricsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            fetch_node_items(client)
                .await
                .map(|items| items.into_iter().map(NodeMetric::from_wire).collect())
        },
        MetricsError::Timeout,
    )
    .await
}

pub async fn fetch_node(
    client: &kube::Client,
    name: &str,
) -> Result<Option<NodeMetric>, MetricsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let path = node_metrics_path(name);
            let request = get(&path)?;
            fetch_single_or_list(
                &path,
                client.request::<NodeMetricsItem>(request),
                fetch_node_items(client),
                |item| item.metadata.name == name,
            )
            .await
            .map(|item| item.map(NodeMetric::from_wire))
        },
        MetricsError::Timeout,
    )
    .await
}

async fn fetch_pod_items(
    client: &kube::Client,
    namespace: Option<&str>,
) -> Result<Vec<PodMetricsItem>, MetricsError> {
    let path = pod_metrics_path(namespace);
    let request = get(&path)?;
    let list: PodMetricsList = client
        .request(request)
        .await
        .map_err(|error| classify_at(&path, error))?;
    Ok(list.items)
}

/// Fetch Pod samples. `None` selects all namespaces.
pub async fn fetch_pods(
    client: &kube::Client,
    namespace: Option<&str>,
) -> Result<Vec<PodMetric>, MetricsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            fetch_pod_items(client, namespace)
                .await
                .map(|items| items.into_iter().map(PodMetric::from_wire).collect())
        },
        MetricsError::Timeout,
    )
    .await
}

pub async fn fetch_pod(
    client: &kube::Client,
    namespace: &str,
    name: &str,
) -> Result<Option<PodMetric>, MetricsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let path = pod_metric_path(namespace, name);
            let request = get(&path)?;
            fetch_single_or_list(
                &path,
                client.request::<PodMetricsItem>(request),
                fetch_pod_items(client, Some(namespace)),
                |item| item.metadata.namespace == namespace && item.metadata.name == name,
            )
            .await
            .map(|item| item.map(PodMetric::from_wire))
        },
        MetricsError::Timeout,
    )
    .await
}

fn node_metrics_path(name: &str) -> String {
    format!("{METRICS_API_PATH}/nodes/{}", encode_path_segment(name))
}

fn pod_metrics_path(namespace: Option<&str>) -> String {
    match namespace {
        Some(namespace) => format!(
            "{METRICS_API_PATH}/namespaces/{}/pods",
            encode_path_segment(namespace)
        ),
        None => format!("{METRICS_API_PATH}/pods"),
    }
}

fn pod_metric_path(namespace: &str, name: &str) -> String {
    format!(
        "{METRICS_API_PATH}/namespaces/{}/pods/{}",
        encode_path_segment(namespace),
        encode_path_segment(name)
    )
}

/// Encode one RFC 3986 path segment.
fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(encoded, "%{other:02X}");
            }
        }
    }
    encoded
}

fn get(path: &str) -> Result<http::Request<Vec<u8>>, MetricsError> {
    http::Request::get(path)
        .body(Vec::new())
        .map_err(|source| MetricsError::Request {
            path: path.to_owned(),
            source,
        })
}

async fn fetch_single_or_list<T, Primary, List>(
    path: &str,
    primary: Primary,
    list: List,
    matches: impl Fn(&T) -> bool,
) -> Result<Option<T>, MetricsError>
where
    Primary: Future<Output = Result<T, kube::Error>>,
    List: Future<Output = Result<Vec<T>, MetricsError>>,
{
    match primary.await {
        Ok(item) => Ok(Some(item)),
        Err(error) if is_not_found(&error) => {
            list.await.map(|items| items.into_iter().find(matches))
        }
        Err(error) => Err(classify_at(path, error)),
    }
}

fn is_not_found(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 404)
}

fn is_forbidden(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 403)
}

/// Classifies one API failure, keeping the request path for a 403.
///
/// The path is what makes the denial actionable: it names the resource whose
/// permission is missing, while a connection failure needs no path at all.
fn classify_at(path: &str, error: kube::Error) -> MetricsError {
    if is_forbidden(&error) {
        return MetricsError::Forbidden {
            path: path.to_owned(),
            permission: metrics_permission(path),
            detail: forbidden_detail(&error),
        };
    }
    classify(error)
}

/// The log text for a denial: the status code, then the API server's own message.
///
/// The code is written here rather than borrowed from `kube`'s `Debug` output,
/// because that field order is an implementation detail of a dependency and a log
/// line an operator greps for `403` must not depend on it.
fn forbidden_detail(error: &kube::Error) -> String {
    match error {
        kube::Error::Api(status) => format!("HTTP {}: {}", status.code, status.message),
        other => other.to_string(),
    }
}

fn classify(error: kube::Error) -> MetricsError {
    match error {
        kube::Error::Api(response) if response.code == 404 => MetricsError::Unavailable,
        other => MetricsError::Api {
            source: Box::new(other),
        },
    }
}

fn sum_values(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total = 0.0;
    let mut seen = false;
    for value in values.flatten() {
        total += value;
        seen = true;
    }
    seen.then_some(total)
}

fn metric_value(usage: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    usage.get(key).and_then(|raw| parse_quantity(raw).ok())
}

#[derive(Debug, Deserialize)]
struct NodeMetricsList {
    #[serde(default)]
    items: Vec<NodeMetricsItem>,
}

#[derive(Debug, Deserialize)]
struct NodeMetricsItem {
    #[serde(default)]
    metadata: ItemMetadata,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    window: String,
    #[serde(default)]
    usage: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct PodMetricsList {
    #[serde(default)]
    items: Vec<PodMetricsItem>,
}

#[derive(Debug, Deserialize)]
struct PodMetricsItem {
    #[serde(default)]
    metadata: ItemMetadata,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    window: String,
    #[serde(default)]
    containers: Vec<ContainerMetricsItem>,
}

#[derive(Debug, Deserialize)]
struct ContainerMetricsItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    usage: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct ItemMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

impl NodeMetric {
    fn from_wire(item: NodeMetricsItem) -> Self {
        Self {
            name: item.metadata.name,
            timestamp: item.timestamp,
            window: item.window,
            cpu_millicores: metric_value(&item.usage, "cpu").map(|cores| cores * 1000.0),
            memory_bytes: metric_value(&item.usage, "memory"),
        }
    }
}

impl PodMetric {
    fn from_wire(item: PodMetricsItem) -> Self {
        Self {
            namespace: item.metadata.namespace,
            name: item.metadata.name,
            timestamp: item.timestamp,
            window: item.window,
            containers: item
                .containers
                .into_iter()
                .map(|container| ContainerMetric {
                    name: container.name,
                    cpu_millicores: metric_value(&container.usage, "cpu")
                        .map(|cores| cores * 1000.0),
                    memory_bytes: metric_value(&container.usage, "memory"),
                })
                .collect(),
        }
    }
}

/// One sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub at_ms: i64,
    pub value: f64,
}

/// Normalized point. `None` marks a missing sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedPoint {
    pub at_ms: i64,
    pub value: Option<f64>,
}

/// Fixed-capacity sample buffer. Timestamps must increase strictly.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeSeries {
    capacity: usize,
    samples: VecDeque<Sample>,
}

impl TimeSeries {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            samples: VecDeque::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Add a sample and report whether it was accepted.
    pub fn push(&mut self, at_ms: i64, value: f64) -> bool {
        if let Some(last) = self.samples.back()
            && at_ms <= last.at_ms
        {
            return false;
        }
        if self.samples.len() >= self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { at_ms, value });
        true
    }

    pub fn latest(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    /// Normalize samples to a fixed interval grid.
    pub fn normalized(&self, interval_ms: i64) -> Vec<NormalizedPoint> {
        let interval = interval_ms.max(1);
        let (Some(first), Some(last)) = (self.samples.front(), self.samples.back()) else {
            return Vec::new();
        };
        let end = align_down(last.at_ms, interval);
        let mut start = align_down(first.at_ms, interval);
        let mut slots = usize::try_from((end - start) / interval + 1).unwrap_or(usize::MAX);
        if slots > MAX_NORMALIZED_SLOTS {
            start = end - (MAX_NORMALIZED_SLOTS as i64 - 1) * interval;
            slots = MAX_NORMALIZED_SLOTS;
        }
        let mut points: Vec<NormalizedPoint> = (0..slots)
            .map(|index| NormalizedPoint {
                at_ms: start + index as i64 * interval,
                value: None,
            })
            .collect();
        for sample in &self.samples {
            if sample.at_ms < start {
                continue;
            }
            let index = usize::try_from((sample.at_ms - start) / interval).unwrap_or(usize::MAX);
            if let Some(point) = points.get_mut(index) {
                point.value = Some(sample.value);
            }
        }
        points
    }
}

fn align_down(at_ms: i64, interval: i64) -> i64 {
    at_ms.div_euclid(interval) * interval
}

/// Sampling decision. The caller owns timing and execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleDecision {
    SampleNow,
    Wait(Duration),
    Stopped,
}

/// Sample when visible. Stop when hidden. Back off after failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleScheduler {
    interval: Duration,
    visible: bool,
    immediate: bool,
    failures: u32,
}

impl Default for SampleScheduler {
    fn default() -> Self {
        Self::new(LatencyTier::Local)
    }
}

impl SampleScheduler {
    pub fn new(tier: LatencyTier) -> Self {
        Self {
            interval: Self::interval_for(tier),
            visible: false,
            immediate: false,
            failures: 0,
        }
    }

    /// Select the sample interval for a tier.
    pub fn interval_for(tier: LatencyTier) -> Duration {
        match tier {
            LatencyTier::Local => SAMPLE_INTERVAL,
            LatencyTier::HighLatency => HIGH_LATENCY_SAMPLE_INTERVAL,
        }
    }

    pub fn set_tier(&mut self, tier: LatencyTier) {
        self.interval = Self::interval_for(tier);
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.failures
    }

    /// Set view visibility. Becoming visible clears backoff and samples next.
    pub fn set_visible(&mut self, visible: bool) {
        if visible && !self.visible {
            self.immediate = true;
            self.failures = 0;
        }
        self.visible = visible;
    }

    /// Wait before the next sample. `None` means stopped.
    fn delay(&self) -> Option<Duration> {
        if !self.visible {
            return None;
        }
        Some(
            self.interval
                .saturating_add(crate::latency::backoff(self.failures)),
        )
    }

    pub fn retry_delay(&self) -> Option<Duration> {
        if self.failures == 0 {
            return None;
        }
        self.delay()
    }

    pub fn decide(&mut self, since_last: Duration) -> SampleDecision {
        let Some(delay) = self.delay() else {
            return SampleDecision::Stopped;
        };
        if self.immediate {
            self.immediate = false;
            return SampleDecision::SampleNow;
        }
        if since_last >= delay {
            SampleDecision::SampleNow
        } else {
            SampleDecision::Wait(delay - since_last)
        }
    }

    pub fn on_success(&mut self) {
        self.failures = 0;
    }

    pub fn on_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= expected.abs().max(1.0) * 1e-9,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn quantity_parses_decimal_and_binary_suffixes() {
        approx(parse_quantity("1").expect("plain number"), 1.0);
        approx(parse_quantity("100m").expect("millicores"), 0.1);
        approx(parse_quantity("1500m").expect("millicores"), 1.5);
        approx(
            parse_quantity("123456789n").expect("nanocores"),
            0.123_456_789,
        );
        approx(parse_quantity("1Ki").expect("Ki"), 1024.0);
        approx(
            parse_quantity("1.5Gi").expect("decimal Gi value"),
            1_610_612_736.0,
        );
        approx(parse_quantity("1Mi").expect("Mi"), 1_048_576.0);
        approx(parse_quantity("1e3").expect("scientific notation"), 1000.0);
        approx(parse_quantity(" 2M ").expect("trim spaces"), 2_000_000.0);
    }

    #[test]
    fn series_rejects_out_of_order_and_evicts_oldest() {
        let mut series = TimeSeries::new(3);
        assert!(series.push(1000, 1.0));
        assert!(!series.push(1000, 2.0), "drop a duplicate timestamp");
        assert!(!series.push(500, 3.0), "drop an out-of-order sample");
        assert!(series.push(2000, 2.0));
        assert!(series.push(3000, 3.0));
        assert!(series.push(4000, 4.0));
        assert_eq!(series.samples.len(), 3);
        assert_eq!(
            series.latest(),
            Some(Sample {
                at_ms: 4000,
                value: 4.0
            })
        );
        let ats: Vec<i64> = series.samples.iter().map(|sample| sample.at_ms).collect();
        assert_eq!(ats, [2000, 3000, 4000], "drop the oldest sample");
    }

    #[test]
    fn normalized_aligns_and_fills_gaps() {
        let mut series = TimeSeries::new(8);
        series.push(100, 1.0);
        series.push(1100, 2.0);
        series.push(3100, 3.0);
        let points = series.normalized(1000);
        let slots: Vec<(i64, Option<f64>)> = points
            .iter()
            .map(|point| (point.at_ms, point.value))
            .collect();
        assert_eq!(
            slots,
            [
                (0, Some(1.0)),
                (1000, Some(2.0)),
                (2000, None),
                (3000, Some(3.0)),
            ]
        );
    }

    #[test]
    fn scheduler_backs_off_on_failures_and_resets_on_success() {
        let mut scheduler = SampleScheduler::new(LatencyTier::Local);
        scheduler.set_visible(true);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);

        scheduler.on_failure();
        assert_eq!(scheduler.consecutive_failures(), 1);
        assert_eq!(scheduler.delay(), Some(Duration::from_secs(11)));
        assert_eq!(scheduler.retry_delay(), Some(Duration::from_secs(11)));
        scheduler.on_failure();
        assert_eq!(scheduler.delay(), Some(Duration::from_secs(12)));
        for _ in 0..10 {
            scheduler.on_failure();
        }
        assert_eq!(
            scheduler.delay(),
            Some(Duration::from_secs(40)),
            "backoff is capped at 30 seconds (10-second interval plus 30 seconds)"
        );
        assert_eq!(scheduler.retry_delay(), scheduler.delay());
        scheduler.on_success();
        assert_eq!(scheduler.consecutive_failures(), 0);
        assert_eq!(scheduler.delay(), Some(Duration::from_secs(10)));
        assert_eq!(scheduler.retry_delay(), None);
    }
}
