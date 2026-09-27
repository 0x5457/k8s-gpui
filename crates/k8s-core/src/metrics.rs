//! Fetches metrics-server samples and normalizes time series.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

use crate::latency::{LatencyTier, NON_WATCH_READ_TIMEOUT, with_read_timeout};

/// Root path for the metrics-server API.
pub const METRICS_API_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

/// Default sample interval for the local tier.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// Sample interval for the high-latency tier.
pub const HIGH_LATENCY_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Backoff base and maximum for failed samples.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Maximum normalized slots. The newest range is kept when the limit is exceeded.
pub const MAX_NORMALIZED_SLOTS: usize = 4096;

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
        "Failed to parse the metrics response: {source}. Check the metrics-server version and try again."
    )]
    Parse {
        #[source]
        source: serde_json::Error,
    },

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
        Some(self.interval.saturating_add(backoff(self.failures)))
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

fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let exponent = failures.saturating_sub(1).min(31);
    BACKOFF_BASE
        .saturating_mul(1u32 << exponent)
        .min(BACKOFF_MAX)
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
    fn quantity_rejects_garbage() {
        for value in ["", "  ", "abc", "m", "Gi", "--1", "1.2.3"] {
            assert!(
                matches!(parse_quantity(value), Err(MetricsError::Quantity { .. })),
                "{value:?} is rejected"
            );
        }
        let message = parse_quantity("??")
            .expect_err("invalid quantity")
            .to_string();
        assert!(message.contains("metrics response"));
        assert!(message.contains("metrics-server"));
        assert!(!message.contains("??"));
    }

    #[test]
    fn timestamp_parses_rfc3339() {
        assert_eq!(
            parse_timestamp_ms("2026-09-23T10:00:00Z"),
            Some(1_790_157_600_000)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-23T18:00:00+08:00"),
            parse_timestamp_ms("2026-09-23T10:00:00Z")
        );
        assert_eq!(parse_timestamp_ms("not a time"), None);
    }

    fn node_item(name: &str, cpu: &str, memory: &str) -> NodeMetricsItem {
        NodeMetricsItem {
            metadata: ItemMetadata {
                name: name.to_owned(),
                namespace: String::new(),
            },
            timestamp: "2026-09-23T10:00:00Z".to_owned(),
            window: "10s".to_owned(),
            usage: BTreeMap::from([
                ("cpu".to_owned(), cpu.to_owned()),
                ("memory".to_owned(), memory.to_owned()),
            ]),
        }
    }

    #[test]
    fn node_wire_is_converted_to_millicores_and_bytes() {
        let metric = NodeMetric::from_wire(node_item("node-a", "250m", "128Mi"));
        assert_eq!(metric.name, "node-a");
        assert_eq!(metric.cpu_millicores, Some(250.0));
        assert_eq!(metric.memory_bytes, Some(134_217_728.0));
        assert_eq!(metric.at_ms(), Some(1_790_157_600_000));
    }

    #[test]
    fn unparsable_usage_degrades_to_none() {
        let metric = NodeMetric::from_wire(node_item("node-a", "wat", "128Mi"));
        assert_eq!(metric.cpu_millicores, None);
        assert_eq!(metric.memory_bytes, Some(134_217_728.0));
    }

    #[test]
    fn missing_usage_degrades_to_none() {
        let mut item = node_item("node-a", "250m", "128Mi");
        item.usage.remove("cpu");
        let metric = NodeMetric::from_wire(item);
        assert_eq!(metric.cpu_millicores, None);
        assert_eq!(metric.memory_bytes, Some(134_217_728.0));
    }

    fn pod_item(containers: &[(&str, Option<&str>, Option<&str>)]) -> PodMetricsItem {
        PodMetricsItem {
            metadata: ItemMetadata {
                name: "web-0".to_owned(),
                namespace: "default".to_owned(),
            },
            timestamp: "2026-09-23T10:00:00Z".to_owned(),
            window: "10s".to_owned(),
            containers: containers
                .iter()
                .map(|(name, cpu, memory)| ContainerMetricsItem {
                    name: (*name).to_owned(),
                    usage: [
                        cpu.map(|value| ("cpu".to_owned(), value.to_owned())),
                        memory.map(|value| ("memory".to_owned(), value.to_owned())),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn pod_totals_sum_containers_and_skip_missing() {
        let metric = PodMetric::from_wire(pod_item(&[
            ("app", Some("100m"), Some("64Mi")),
            ("sidecar", Some("20m"), None),
        ]));
        assert_eq!(metric.namespace, "default");
        assert_eq!(metric.cpu_millicores(), Some(120.0));
        assert_eq!(metric.memory_bytes(), Some(67_108_864.0));
    }

    #[test]
    fn pod_totals_are_none_without_any_value() {
        let metric = PodMetric::from_wire(pod_item(&[("app", None, None)]));
        assert_eq!(metric.cpu_millicores(), None);
        assert_eq!(metric.memory_bytes(), None);
    }

    #[test]
    fn metric_paths_use_single_object_endpoints_and_encode_names() {
        assert_eq!(pod_metrics_path(None), "/apis/metrics.k8s.io/v1beta1/pods");
        assert_eq!(
            pod_metrics_path(Some("kube-system")),
            "/apis/metrics.k8s.io/v1beta1/namespaces/kube-system/pods"
        );
        assert_eq!(
            node_metrics_path("node-a"),
            "/apis/metrics.k8s.io/v1beta1/nodes/node-a"
        );
        assert_eq!(
            node_metrics_path("node/a"),
            "/apis/metrics.k8s.io/v1beta1/nodes/node%2Fa"
        );
        assert_eq!(
            pod_metric_path("team-a", "pod-a"),
            "/apis/metrics.k8s.io/v1beta1/namespaces/team-a/pods/pod-a"
        );
        assert_eq!(
            pod_metric_path("weird ns/1", "pod/a"),
            "/apis/metrics.k8s.io/v1beta1/namespaces/weird%20ns%2F1/pods/pod%2Fa"
        );
    }

    #[test]
    fn api_404_maps_to_unavailable() {
        let not_found = kube::Error::Api(Box::new(kube::core::Status {
            message: "the server did not find the requested resource".to_owned(),
            reason: "NotFound".to_owned(),
            code: 404,
            ..Default::default()
        }));
        assert!(is_not_found(&not_found));
        assert!(matches!(classify(not_found), MetricsError::Unavailable));

        let server_error = kube::Error::Api(Box::new(kube::core::Status {
            message: "boom".to_owned(),
            reason: "InternalError".to_owned(),
            code: 500,
            ..Default::default()
        }));
        assert!(!is_not_found(&server_error));
        assert!(matches!(classify(server_error), MetricsError::Api { .. }));
    }

    /// A 403 is the cluster answering, so it must name the missing permission
    /// instead of sending the user after the connection.
    #[test]
    fn api_403_maps_to_forbidden_with_the_missing_permission() {
        let denied = kube::Error::Api(Box::new(kube::core::Status {
            message: "nodes.metrics.k8s.io is forbidden".to_owned(),
            reason: "Forbidden".to_owned(),
            code: 403,
            ..Default::default()
        }));
        assert!(is_forbidden(&denied));
        assert!(!is_not_found(&denied));

        let path = format!("{METRICS_API_PATH}/nodes");
        let error = classify_at(&path, denied);
        assert!(error.is_forbidden());
        assert!(!error.is_unavailable());
        assert_eq!(error.permission(), Some("list nodes.metrics.k8s.io"));
        let copy = error.to_string();
        assert!(copy.contains("denied"));
        assert!(
            copy.contains("list nodes.metrics.k8s.io"),
            "the copy names the missing permission: {copy}"
        );
        assert!(
            copy.contains("403"),
            "the API text stays available for logs: {copy}"
        );
        assert!(
            !copy.contains("Status {"),
            "the log text does not lean on kube's Debug layout: {copy}"
        );

        let server_error = kube::Error::Api(Box::new(kube::core::Status {
            message: "boom".to_owned(),
            reason: "InternalError".to_owned(),
            code: 500,
            ..Default::default()
        }));
        let other = classify_at(&path, server_error);
        assert!(!other.is_forbidden());
        assert_eq!(other.permission(), None);
    }

    #[test]
    fn metrics_permission_names_the_resource_and_scope() {
        assert_eq!(
            metrics_permission(METRICS_API_PATH),
            "get the metrics.k8s.io API discovery endpoint"
        );
        assert_eq!(
            metrics_permission(&format!("{METRICS_API_PATH}/nodes")),
            "list nodes.metrics.k8s.io"
        );
        assert_eq!(
            metrics_permission(&format!("{METRICS_API_PATH}/pods")),
            "list pods.metrics.k8s.io"
        );
        assert_eq!(
            metrics_permission(&format!("{METRICS_API_PATH}/nodes/worker-a")),
            "list nodes.metrics.k8s.io"
        );
        assert_eq!(
            metrics_permission(&pod_metric_path("kube-system", "web-0")),
            "list pods.metrics.k8s.io in the kube-system namespace"
        );
        assert_eq!(
            metrics_permission(&pod_metrics_path(None)),
            "list pods.metrics.k8s.io"
        );
    }

    #[tokio::test]
    async fn single_object_404_falls_back_to_matching_list_item() {
        let not_found = kube::Error::Api(Box::new(kube::core::Status {
            message: "single-object endpoint not found".to_owned(),
            reason: "NotFound".to_owned(),
            code: 404,
            ..Default::default()
        }));
        let metric = fetch_single_or_list(
            &format!("{METRICS_API_PATH}/nodes/node-b"),
            async move { Err(not_found) },
            async {
                Ok(vec![
                    node_item("node-a", "100m", "64Mi"),
                    node_item("node-b", "250m", "128Mi"),
                ])
            },
            |item| item.metadata.name == "node-b",
        )
        .await
        .expect("list fallback")
        .expect("matching node");

        assert_eq!(metric.metadata.name, "node-b");
        assert_eq!(metric.usage.get("cpu").map(String::as_str), Some("250m"));
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
    fn normalized_last_value_wins_within_a_slot() {
        let mut series = TimeSeries::new(8);
        series.push(100, 1.0);
        series.push(900, 9.0);
        let points = series.normalized(1000);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value, Some(9.0));
    }

    #[test]
    fn normalized_caps_slots_keeping_newest() {
        let mut series = TimeSeries::new(2);
        series.push(0, 1.0);
        let last = (MAX_NORMALIZED_SLOTS as i64 + 100) * 1000;
        series.push(last, 2.0);
        let points = series.normalized(1000);
        assert_eq!(points.len(), MAX_NORMALIZED_SLOTS);
        assert_eq!(
            points.last().map(|point| point.at_ms),
            Some(align_down(last, 1000))
        );
    }

    #[test]
    fn normalized_empty_series_is_empty() {
        assert!(TimeSeries::new(4).normalized(1000).is_empty());
    }

    #[test]
    fn scheduler_interval_follows_tier() {
        let mut scheduler = SampleScheduler::new(LatencyTier::Local);
        assert_eq!(scheduler.interval(), Duration::from_secs(10));
        scheduler.set_tier(LatencyTier::HighLatency);
        assert_eq!(scheduler.interval(), Duration::from_secs(30));
        assert_eq!(
            SampleScheduler::interval_for(LatencyTier::HighLatency),
            HIGH_LATENCY_SAMPLE_INTERVAL
        );
    }

    #[test]
    fn scheduler_stops_when_invisible() {
        let mut scheduler = SampleScheduler::new(LatencyTier::Local);
        assert_eq!(
            scheduler.decide(Duration::from_secs(3600)),
            SampleDecision::Stopped
        );
        assert_eq!(scheduler.delay(), None);
    }

    #[test]
    fn scheduler_samples_immediately_when_becoming_visible() {
        let mut scheduler = SampleScheduler::new(LatencyTier::Local);
        scheduler.set_visible(true);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
        assert_eq!(
            scheduler.decide(Duration::ZERO),
            SampleDecision::Wait(Duration::from_secs(10)),
            "immediate sampling applies once"
        );
        assert_eq!(
            scheduler.decide(Duration::from_secs(3)),
            SampleDecision::Wait(Duration::from_secs(7))
        );
        assert_eq!(
            scheduler.decide(Duration::from_secs(10)),
            SampleDecision::SampleNow
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

    #[test]
    fn scheduler_hiding_clears_immediate_and_backoff() {
        let mut scheduler = SampleScheduler::new(LatencyTier::Local);
        scheduler.set_visible(true);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
        scheduler.on_failure();
        scheduler.set_visible(false);
        assert_eq!(
            scheduler.decide(Duration::from_secs(60)),
            SampleDecision::Stopped
        );
        scheduler.set_visible(true);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
        assert_eq!(scheduler.delay(), Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    #[ignore = "Requires kind and metrics-server: KUBECONFIG or ~/.kube/config"]
    async fn fetches_real_cluster_metrics() {
        if !crate::cluster::kubeconfig_present() {
            return;
        }
        let registry = crate::cluster::ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        // Select the first cluster with metrics-server.
        let mut available = None;
        for cluster in registry.clusters() {
            match probe(cluster.client()).await {
                Ok(()) => {
                    available = Some(cluster);
                    break;
                }
                Err(error) => eprintln!("Skipping cluster {}: {error}", cluster.name()),
            }
        }
        let Some(cluster) = available else {
            eprintln!("No cluster with metrics-server is available. Skipping.");
            return;
        };
        eprintln!("Using cluster {}", cluster.name());
        let nodes = fetch_nodes(cluster.client()).await.expect("node sample");
        assert!(!nodes.is_empty(), "at least one node");
        let node = &nodes[0];
        eprintln!(
            "node {} cpu={:?}m memory={:?}B at {:?}",
            node.name,
            node.cpu_millicores,
            node.memory_bytes,
            node.at_ms()
        );
        assert!(node.cpu_millicores.is_some_and(|cpu| cpu > 0.0));
        assert!(node.memory_bytes.is_some_and(|bytes| bytes > 0.0));

        let pods = fetch_pods(cluster.client(), None)
            .await
            .expect("Pod sample");
        eprintln!("Pod metrics: {}", pods.len());
        assert!(pods.iter().all(|pod| pod.at_ms().is_some()));
    }
}
