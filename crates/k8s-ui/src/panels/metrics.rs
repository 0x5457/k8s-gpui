//! Metrics sampling and chart data for the Inspector.
//! Sampling runs while the Metrics tab is visible.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use gpui::SharedString;
use k8s_core::latency::{Latency, LatencyTier};
use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric, SampleScheduler, TimeSeries};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::charts::{ChartData, Series, SeriesColor, Unit};
use crate::session::OpsFuture;
use k8s_core::cluster_data::ClusterDataSource;

/// Fastest sampling interval across latency tiers, in milliseconds.
const FASTEST_SAMPLE_INTERVAL_MS: i64 = 10_000;
/// Samples retained per series.
///
/// The buffer must cover the longest selectable range at the fastest interval, otherwise
/// picking that range shows only the samples that happen to still be in memory.
pub const METRICS_CAPACITY: usize = MAX_RANGE_MS as usize / FASTEST_SAMPLE_INTERVAL_MS as usize + 2;
const METRICS_SERIES_GRACE_SAMPLES: u8 = 1;

pub const METRICS_UNAVAILABLE: &str =
    "Metrics are not available in this cluster. Install or enable metrics-server, then retry.";

/// User-facing copy for request and sampling failures.
pub const METRICS_REQUEST_FAILED: &str =
    "Failed to read metrics from the cluster. Retry, or make sure the cluster connection works.";

/// Opens the copy for an access denial, so `from_result` can tell a denial
/// apart from a connection failure on the error channel it receives.
const FORBIDDEN_COPY_PREFIX: &str = "The cluster denied access to metrics";

/// User-facing copy for an RBAC denial.
///
/// The permission is named because a denial is a configuration problem: the
/// cluster answered, so telling the user to check the connection sends them
/// after the wrong thing.
pub fn forbidden_copy(permission: &str) -> String {
    format!("{FORBIDDEN_COPY_PREFIX}. Grant the {permission} permission, then retry.")
}

/// Title for the denial state in the Metrics panel.
pub const FORBIDDEN_TITLE: &str = "Access to metrics is denied";

/// Next step for the denial state in the Metrics panel.
pub const FORBIDDEN_HINT: &str =
    "The cluster refused to read metrics. Grant the permission below, then retry.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MetricsProbeState {
    #[default]
    Checking,
    Available,
    Missing,
    /// The cluster denied the request. `reason` names the missing permission.
    Forbidden {
        reason: String,
    },
    Error {
        reason: String,
    },
}

impl MetricsProbeState {
    pub fn from_result(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self::Available,
            Err(reason) if reason == METRICS_UNAVAILABLE => Self::Missing,
            // The probe reports a String, so the denial is recognised by the one
            // copy `map_error` writes for it. Keeping both in this file is what
            // makes the mapping total.
            Err(reason) if reason.starts_with(FORBIDDEN_COPY_PREFIX) => Self::Forbidden { reason },
            Err(reason) => Self::Error { reason },
        }
    }

    /// Builds the denial state from a missing RBAC permission.
    pub fn forbidden(permission: &str) -> Self {
        Self::Forbidden {
            reason: forbidden_copy(permission),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// True when the cluster denied the request instead of failing to answer.
    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Forbidden { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Missing => Some(METRICS_UNAVAILABLE),
            // The copy is written here, not taken from the error, so no raw API
            // text can reach the surface.
            Self::Forbidden { reason } => Some(reason),
            Self::Error { .. } => Some(METRICS_REQUEST_FAILED),
            Self::Checking | Self::Available => None,
        }
    }
}

/// Default chart range: 15 minutes.
pub const DEFAULT_RANGE_MS: i64 = 15 * 60 * 1000;

/// Chart and table range options in milliseconds.
pub const RANGE_OPTIONS: [(i64, &str); 3] = [
    (5 * 60 * 1000, "5m"),
    (15 * 60 * 1000, "15m"),
    (60 * 60 * 1000, "1h"),
];

/// Longest selectable chart range in milliseconds.
pub const MAX_RANGE_MS: i64 = RANGE_OPTIONS[RANGE_OPTIONS.len() - 1].0;

/// Maps errors to user-facing copy and logs the original detail.
fn map_error(error: MetricsError) -> String {
    if error.is_unavailable() {
        METRICS_UNAVAILABLE.to_owned()
    } else if let Some(permission) = error.permission() {
        // A denial names the permission. It never falls through to the
        // connection copy, which would be the wrong advice for an RBAC problem.
        eprintln!("k8s-gpui: metrics request denied: {error}");
        forbidden_copy(permission)
    } else {
        request_failure(error)
    }
}

/// Logs background request failures and returns user-facing copy.
fn request_failure(detail: impl std::fmt::Display) -> String {
    eprintln!("k8s-gpui: metrics request failed: {detail}");
    METRICS_REQUEST_FAILED.to_owned()
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn join_abortable<T>(
    handle: &Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
{
    let task = handle.spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    task.await
}

/// Metrics data source handle.
#[derive(Clone)]
pub struct MetricsHandle {
    handle: Handle,
    service: ClusterDataSource,
    latency: Option<watch::Receiver<Latency>>,
}

impl MetricsHandle {
    pub fn new(handle: Handle, source: impl Into<ClusterDataSource>) -> Self {
        Self {
            handle,
            service: source.into(),
            latency: None,
        }
    }

    pub fn with_latency(mut self, latency: watch::Receiver<Latency>) -> Self {
        self.latency = Some(latency);
        self
    }

    pub fn tier(&self) -> LatencyTier {
        self.latency
            .as_ref()
            .map(|latency| latency.borrow().tier())
            .unwrap_or(LatencyTier::Local)
    }

    pub fn latency_receiver(&self) -> Option<watch::Receiver<Latency>> {
        self.latency.clone()
    }

    /// Checks whether metrics-server is available.
    pub fn probe_future(&self) -> OpsFuture<()> {
        let handle = self.handle.clone();
        let service = self.service.clone();
        Box::pin(async move {
            join_abortable(&handle, async move { service.port().metrics_probe().await })
                .await
                .map_err(request_failure)?
                .map_err(map_error)
        })
    }

    pub fn node_future(&self, name: &str) -> OpsFuture<SamplePayload> {
        let handle = self.handle.clone();
        let service = self.service.clone();
        let name = name.to_owned();
        let task_name = name.clone();
        Box::pin(async move {
            let metric = join_abortable(
                &handle,
                async move { service.metrics_node(task_name).await },
            )
            .await
            .map_err(request_failure)?
            .map_err(map_error)?;
            metric.map(SamplePayload::from_node).ok_or_else(|| {
                format!(
                    "The node {name} has no metrics. Make sure metrics-server is running, then retry."
                )
            })
        })
    }

    pub fn pod_future(&self, namespace: &str, name: &str) -> OpsFuture<SamplePayload> {
        let handle = self.handle.clone();
        let service = self.service.clone();
        let namespace = namespace.to_owned();
        let name = name.to_owned();
        let task_namespace = namespace.clone();
        let task_name = name.clone();
        Box::pin(async move {
            let metric = join_abortable(&handle, async move {
                service.metrics_pod(task_namespace, task_name).await
            })
            .await
            .map_err(request_failure)?
            .map_err(map_error)?;
            metric.map(SamplePayload::from_pod).ok_or_else(|| {
                format!(
                    "The pod {namespace}/{name} has no metrics. Make sure metrics-server is running, then retry."
                )
            })
        })
    }
}

/// Metrics target for the current Inspector selection.
///
/// The UID is part of the identity: a Pod recreated under the same name must not keep
/// the samples of its predecessor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetricsTarget {
    Node {
        name: String,
        uid: String,
    },
    Pod {
        namespace: String,
        name: String,
        uid: String,
    },
}

impl MetricsTarget {
    /// Derives a target from the selected object.
    pub fn from_object(kind: &str, namespace: Option<&str>, name: &str, uid: &str) -> Option<Self> {
        match kind {
            "Node" => Some(Self::Node {
                name: name.to_owned(),
                uid: uid.to_owned(),
            }),
            "Pod" => Some(Self::Pod {
                namespace: namespace.unwrap_or("default").to_owned(),
                name: name.to_owned(),
                uid: uid.to_owned(),
            }),
            _ => None,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Node { .. } => "Node",
            Self::Pod { .. } => "Pod",
        }
    }

    pub fn title(&self) -> SharedString {
        match self {
            Self::Node { name, .. } => name.clone().into(),
            Self::Pod { name, .. } => name.clone().into(),
        }
    }

    /// Identity of the object whose metrics are charted.
    pub fn uid(&self) -> &str {
        match self {
            Self::Node { uid, .. } | Self::Pod { uid, .. } => uid,
        }
    }
}

/// One normalized sample in millicores and bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplePayload {
    pub containers: Vec<ContainerSample>,
    pub window: String,
    pub at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerSample {
    pub name: String,
    pub cpu_millicores: Option<f64>,
    pub memory_bytes: Option<f64>,
}

impl SamplePayload {
    fn from_node(metric: NodeMetric) -> Self {
        let at_ms = metric.at_ms().unwrap_or_else(now_ms);
        Self {
            containers: vec![ContainerSample {
                name: metric.name,
                cpu_millicores: metric.cpu_millicores,
                memory_bytes: metric.memory_bytes,
            }],
            window: metric.window,
            at_ms,
        }
    }

    fn from_pod(metric: PodMetric) -> Self {
        let at_ms = metric.at_ms().unwrap_or_else(now_ms);
        let total = (metric.cpu_millicores(), metric.memory_bytes());
        let mut containers: Vec<ContainerSample> = metric
            .containers
            .into_iter()
            .map(|container| ContainerSample {
                name: container.name,
                cpu_millicores: container.cpu_millicores,
                memory_bytes: container.memory_bytes,
            })
            .collect();
        if containers.len() > 1 {
            containers.insert(
                0,
                ContainerSample {
                    name: "Total".to_owned(),
                    cpu_millicores: total.0,
                    memory_bytes: total.1,
                },
            );
        }
        Self {
            containers,
            window: metric.window,
            at_ms,
        }
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// A named CPU and memory time series.
#[derive(Clone, Debug)]
pub struct MetricSeries {
    pub name: String,
    pub cpu: TimeSeries,
    pub memory: TimeSeries,
}

impl MetricSeries {
    fn new(name: String) -> Self {
        Self {
            name,
            cpu: TimeSeries::new(METRICS_CAPACITY),
            memory: TimeSeries::new(METRICS_CAPACITY),
        }
    }

    pub fn latest_cpu(&self) -> Option<f64> {
        self.cpu.latest().map(|sample| sample.value)
    }

    pub fn latest_memory(&self) -> Option<f64> {
        self.memory.latest().map(|sample| sample.value)
    }
}

/// Metrics data for the Inspector tab.
#[derive(Clone, Debug, Default)]
pub struct MetricsSamples {
    pub series: Vec<MetricSeries>,
    pub window: String,
    pub last_error: Option<String>,
    /// Consecutive samples in which a series was missing.
    missing: HashMap<String, u8>,
}

impl MetricsSamples {
    pub fn clear(&mut self) {
        self.series.clear();
        self.window.clear();
        self.last_error = None;
        self.missing.clear();
    }

    /// A series counts as data when it carries CPU or memory samples.
    ///
    /// Some clusters report memory only, and a container can lose its CPU sample for a
    /// while. Looking at CPU alone would hide that data behind the waiting state.
    pub fn is_empty(&self) -> bool {
        self.series
            .iter()
            .all(|series| series.cpu.is_empty() && series.memory.is_empty())
    }

    /// Records one sample and creates a series for each new container.
    pub fn record(&mut self, payload: SamplePayload) {
        let latest = self
            .series
            .iter()
            .flat_map(|series| {
                [series.cpu.latest(), series.memory.latest()]
                    .into_iter()
                    .flatten()
            })
            .map(|sample| sample.at_ms)
            .max();
        if latest.is_some_and(|latest| payload.at_ms <= latest) {
            return;
        }
        let active = payload
            .containers
            .iter()
            .map(|container| container.name.clone())
            .collect::<HashSet<_>>();
        let mut removed = HashSet::new();
        for series in &self.series {
            if active.contains(&series.name) {
                self.missing.remove(&series.name);
            } else {
                let missing = self.missing.entry(series.name.clone()).or_default();
                *missing = missing.saturating_add(1);
                if *missing > METRICS_SERIES_GRACE_SAMPLES {
                    removed.insert(series.name.clone());
                }
            }
        }
        self.series.retain(|series| !removed.contains(&series.name));
        for name in removed {
            self.missing.remove(&name);
        }
        for container in payload.containers {
            let position = self
                .series
                .iter()
                .position(|series| series.name == container.name)
                .unwrap_or_else(|| {
                    self.series.push(MetricSeries::new(container.name.clone()));
                    self.series.len() - 1
                });
            let series = &mut self.series[position];
            if let Some(cpu) = container.cpu_millicores {
                series.cpu.push(payload.at_ms, cpu);
            }
            if let Some(memory) = container.memory_bytes {
                series.memory.push(payload.at_ms, memory);
            }
        }
        self.window = payload.window;
        self.last_error = None;
    }

    fn chart_data(&self, unit: Unit, range_ms: i64, interval_ms: i64) -> ChartData {
        let mut data = ChartData::new(interval_ms);
        let cutoff = self
            .series
            .iter()
            .flat_map(|series| match unit {
                Unit::Cpu => series.cpu.latest(),
                Unit::Memory => series.memory.latest(),
                Unit::Count => series.cpu.latest(),
            })
            .map(|sample| sample.at_ms)
            .max()
            .map(|latest| latest - range_ms);
        for (index, series) in self.series.iter().enumerate() {
            let raw = match unit {
                Unit::Memory => &series.memory,
                _ => &series.cpu,
            };
            if raw.is_empty() {
                continue;
            }
            let mut chart_series = Series::from_time_series(
                series.name.clone(),
                unit,
                SeriesColor::for_index(index),
                raw,
                interval_ms,
            );
            chart_series.filled = self.series.len() == 1;
            if let Some(cutoff) = cutoff {
                chart_series.points.retain(|point| point.at_ms >= cutoff);
            }
            data.push_series(chart_series);
        }
        data
    }

    pub fn cpu_chart_data(&self, range_ms: i64, interval_ms: i64) -> ChartData {
        self.chart_data(Unit::Cpu, range_ms, interval_ms)
    }

    pub fn memory_chart_data(&self, range_ms: i64, interval_ms: i64) -> ChartData {
        self.chart_data(Unit::Memory, range_ms, interval_ms)
    }

    /// Returns the sampling interval in milliseconds.
    pub fn interval_ms(scheduler: &SampleScheduler) -> i64 {
        i64::try_from(scheduler.interval().as_millis()).unwrap_or(i64::MAX)
    }

    pub fn scheduler_for_tier(tier: LatencyTier) -> SampleScheduler {
        let mut scheduler = SampleScheduler::new(tier);
        scheduler.set_visible(true);
        scheduler
    }

    pub fn default_scheduler() -> SampleScheduler {
        Self::scheduler_for_tier(LatencyTier::Local)
    }
}

pub fn sampling_interval_text(interval: Duration) -> String {
    format!("New samples arrive every {} seconds.", interval.as_secs())
}

/// Copy for this app's own sampling cadence.
///
/// The metrics-server window is the server's own smoothing period. It is a
/// different number from the rate at which the Inspector samples, and the
/// toolbar has to show both or the window reads as the app's refresh rate.
pub fn sampling_cadence_text(interval: Duration) -> String {
    format!("Sampling every {}s", interval.as_secs())
}

pub fn retry_delay_text(scheduler: &SampleScheduler) -> Option<String> {
    scheduler
        .retry_delay()
        .map(|delay| format!("Retrying in {} seconds", delay.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_core::cluster_data::{ClusterDataPort, DataFuture};
    use k8s_core::latency::LatencyTier;
    use k8s_core::metrics::{SampleDecision, parse_timestamp_ms};
    use k8s_core::overview::Overview;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    struct PendingUntilDropped(Arc<AtomicBool>);

    impl Future for PendingUntilDropped {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct ObjectMetricsPort {
        node_requests: AtomicUsize,
        pod_requests: AtomicUsize,
        list_requests: AtomicUsize,
    }

    impl ClusterDataPort for ObjectMetricsPort {
        fn overview(&self, _metrics: bool) -> DataFuture<Overview, String> {
            Box::pin(async { Ok(Overview::default()) })
        }

        fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
            Box::pin(async { Ok(()) })
        }

        fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
            self.list_requests.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(Vec::new()) })
        }

        fn metrics_pods(
            &self,
            _namespace: Option<String>,
        ) -> DataFuture<Vec<PodMetric>, MetricsError> {
            self.list_requests.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(Vec::new()) })
        }

        fn metrics_node(
            self: Arc<Self>,
            name: String,
        ) -> DataFuture<Option<NodeMetric>, MetricsError> {
            self.node_requests.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(Some(NodeMetric {
                    name,
                    timestamp: "2026-09-23T10:00:00Z".to_owned(),
                    window: "10s".to_owned(),
                    cpu_millicores: Some(100.0),
                    memory_bytes: Some(1_024.0),
                }))
            })
        }

        fn metrics_pod(
            self: Arc<Self>,
            namespace: String,
            name: String,
        ) -> DataFuture<Option<PodMetric>, MetricsError> {
            self.pod_requests.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(Some(PodMetric {
                    namespace,
                    name,
                    timestamp: "2026-09-23T10:00:00Z".to_owned(),
                    window: "10s".to_owned(),
                    containers: vec![k8s_core::metrics::ContainerMetric {
                        name: "app".to_owned(),
                        cpu_millicores: Some(100.0),
                        memory_bytes: Some(1_024.0),
                    }],
                }))
            })
        }

        fn namespaces(&self) -> DataFuture<Vec<String>, String> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cluster_uid(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    fn payload(at_ms: i64, containers: &[(&str, f64, f64)]) -> SamplePayload {
        SamplePayload {
            containers: containers
                .iter()
                .map(|(name, cpu, memory)| ContainerSample {
                    name: (*name).to_owned(),
                    cpu_millicores: Some(*cpu),
                    memory_bytes: Some(*memory),
                })
                .collect(),
            window: "10s".to_owned(),
            at_ms,
        }
    }

    #[tokio::test]
    async fn dropping_finite_request_aborts_inner_task() {
        let dropped = Arc::new(AtomicBool::new(false));
        let handle = Handle::current();
        let request = join_abortable(&handle, PendingUntilDropped(Arc::clone(&dropped)));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), request)
                .await
                .is_err()
        );
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn object_futures_use_single_object_ports() {
        let port = Arc::new(ObjectMetricsPort::default());
        let source: Arc<dyn ClusterDataPort> = port.clone();
        let handle = MetricsHandle::new(Handle::current(), source);

        let node = handle.node_future("node-a").await.expect("node sample");
        let pod = handle
            .pod_future("default", "pod-a")
            .await
            .expect("Pod sample");

        assert_eq!(node.containers[0].name, "node-a");
        assert_eq!(pod.containers[0].name, "app");
        assert_eq!(port.node_requests.load(Ordering::Relaxed), 1);
        assert_eq!(port.pod_requests.load(Ordering::Relaxed), 1);
        assert_eq!(port.list_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn records_series_and_builds_chart_data() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(1_000, &[("app", 100.0, 1024.0)]));
        samples.record(payload(11_000, &[("app", 200.0, 2048.0)]));
        assert_eq!(samples.series.len(), 1);
        let series = &samples.series[0];
        assert_eq!(series.latest_cpu(), Some(200.0));
        assert_eq!(series.latest_memory(), Some(2048.0));

        let data = samples.cpu_chart_data(DEFAULT_RANGE_MS, 10_000);
        assert_eq!(data.series.len(), 1);
        assert_eq!(data.unit(), Unit::Cpu);
        assert!(data.time_range().is_some());
    }

    #[test]
    fn stale_metric_update_does_not_replace_newer_sample() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(20_000, &[("app", 20.0, 2_048.0)]));
        samples.last_error = Some("retry".to_owned());

        samples.record(payload(10_000, &[("stale", 10.0, 1_024.0)]));

        assert_eq!(samples.series.len(), 1);
        assert_eq!(samples.series[0].name, "app");
        assert_eq!(samples.series[0].latest_cpu(), Some(20.0));
        assert_eq!(samples.series[0].latest_memory(), Some(2_048.0));
        assert_eq!(samples.last_error.as_deref(), Some("retry"));
    }

    #[test]
    fn new_container_appends_a_series() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(1_000, &[("app", 100.0, 1024.0)]));
        samples.record(payload(
            11_000,
            &[("app", 100.0, 1024.0), ("sidecar", 5.0, 64.0)],
        ));
        assert_eq!(samples.series.len(), 2);
        assert_eq!(samples.series[1].name, "sidecar");
    }

    #[test]
    fn prunes_series_missing_after_grace() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(1_000, &[("app", 100.0, 1024.0)]));
        samples.record(payload(
            11_000,
            &[("app", 100.0, 1024.0), ("sidecar", 5.0, 64.0)],
        ));
        samples.record(payload(21_000, &[("app", 100.0, 1024.0)]));
        assert_eq!(samples.series.len(), 2);
        samples.record(payload(31_000, &[("app", 100.0, 1024.0)]));
        assert_eq!(
            samples
                .series
                .iter()
                .map(|series| series.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app"]
        );
    }

    #[test]
    fn clear_resets_series_and_grace_state() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(1_000, &[("app", 100.0, 1024.0)]));
        samples.clear();
        assert!(samples.missing.is_empty());
        samples.record(payload(2_000, &[("other", 1.0, 2.0)]));
        assert_eq!(samples.series.len(), 1);
        assert_eq!(samples.series[0].name, "other");
    }

    #[test]
    fn memory_only_sample_is_not_reported_as_empty() {
        let mut samples = MetricsSamples::default();
        samples.record(SamplePayload {
            containers: vec![ContainerSample {
                name: "app".to_owned(),
                cpu_millicores: None,
                memory_bytes: Some(2_048.0),
            }],
            window: "10s".to_owned(),
            at_ms: 1_000,
        });
        assert!(
            !samples.is_empty(),
            "memory data must not look like a missing first sample"
        );
        let memory = samples.memory_chart_data(DEFAULT_RANGE_MS, 10_000);
        assert_eq!(memory.series.len(), 1);
        let cpu = samples.cpu_chart_data(DEFAULT_RANGE_MS, 10_000);
        assert!(
            cpu.series.is_empty(),
            "a container without CPU samples has no CPU line"
        );
    }

    #[test]
    fn capacity_covers_the_longest_range_at_the_fastest_interval() {
        let covered_ms = METRICS_CAPACITY as i64 * FASTEST_SAMPLE_INTERVAL_MS;
        assert!(
            covered_ms >= MAX_RANGE_MS,
            "{METRICS_CAPACITY} samples cover {covered_ms} ms, the longest range is {MAX_RANGE_MS} ms"
        );
    }

    #[test]
    fn range_filters_old_points() {
        let mut samples = MetricsSamples::default();
        samples.record(payload(1_000, &[("app", 1.0, 1.0)]));
        samples.record(payload(601_000, &[("app", 2.0, 2.0)]));
        let data = samples.cpu_chart_data(60_000, 10_000);
        let points = &data.series[0].points;
        assert!(
            points.iter().all(|point| point.at_ms >= 541_000),
            "Only keep points in the time window: {points:?}"
        );
        assert_eq!(
            points.last().map(|point| point.at_ms),
            Some(600_000),
            "Keep the newest point at the window end (align the grid down)"
        );
    }

    #[test]
    fn pod_payload_prepends_total_for_multiple_containers() {
        let metric = PodMetric {
            namespace: "default".to_owned(),
            name: "web-0".to_owned(),
            timestamp: "2026-09-23T10:00:00Z".to_owned(),
            window: "10s".to_owned(),
            containers: vec![
                k8s_core::metrics::ContainerMetric {
                    name: "app".to_owned(),
                    cpu_millicores: Some(100.0),
                    memory_bytes: Some(64.0),
                },
                k8s_core::metrics::ContainerMetric {
                    name: "sidecar".to_owned(),
                    cpu_millicores: Some(20.0),
                    memory_bytes: None,
                },
            ],
        };
        let payload = SamplePayload::from_pod(metric);
        assert_eq!(payload.containers[0].name, "Total");
        assert_eq!(payload.containers[0].cpu_millicores, Some(120.0));
        assert_eq!(payload.containers[0].memory_bytes, Some(64.0));
        assert_eq!(payload.containers[1].name, "app");
        assert_eq!(payload.at_ms, 1_790_157_600_000);
    }

    #[test]
    fn single_container_pod_has_no_total() {
        let metric = PodMetric {
            namespace: "default".to_owned(),
            name: "web-0".to_owned(),
            timestamp: "2026-09-23T10:00:00Z".to_owned(),
            window: "10s".to_owned(),
            containers: vec![k8s_core::metrics::ContainerMetric {
                name: "app".to_owned(),
                cpu_millicores: Some(100.0),
                memory_bytes: Some(64.0),
            }],
        };
        let payload = SamplePayload::from_pod(metric);
        assert_eq!(payload.containers.len(), 1);
        assert_eq!(payload.containers[0].name, "app");
    }

    #[test]
    fn probe_state_distinguishes_missing_from_error() {
        assert_eq!(
            MetricsProbeState::from_result(Ok(())),
            MetricsProbeState::Available
        );
        let missing = MetricsProbeState::from_result(Err(METRICS_UNAVAILABLE.to_owned()));
        assert_eq!(missing, MetricsProbeState::Missing);
        assert_eq!(missing.reason(), Some(METRICS_UNAVAILABLE));
        assert_eq!(
            MetricsProbeState::from_result(Err("connection refused".to_owned())),
            MetricsProbeState::Error {
                reason: "connection refused".to_owned()
            }
        );
    }

    /// A denial and a connection failure are different problems with different
    /// next steps, so they must not share a state or a sentence.
    #[test]
    fn rbac_denial_is_distinct_from_a_connection_failure() {
        let denied = MetricsProbeState::from_result(Err(map_error(MetricsError::Forbidden {
            path: "/apis/metrics.k8s.io/v1beta1/nodes".to_owned(),
            permission: "list nodes.metrics.k8s.io".to_owned(),
            detail: "ApiError: nodes.metrics.k8s.io is forbidden (Status { code: 403 })".to_owned(),
        })));
        assert!(
            denied.is_forbidden(),
            "a 403 must not land in the connection-failure state: {denied:?}"
        );
        assert!(!denied.is_available());
        let copy = denied.reason().expect("a denial explains itself");
        assert!(copy.contains("list nodes.metrics.k8s.io"));
        assert!(copy.contains("Grant"));
        for internal in ["403", "Status", "ApiError", "/apis/"] {
            assert!(
                !copy.contains(internal),
                "the raw API text must not reach the copy: {copy}"
            );
        }

        let unreachable = MetricsProbeState::from_result(Err(
            "Failed to read metrics from the cluster. Retry, or make sure the cluster connection works."
                .to_owned(),
        ));
        assert!(
            !unreachable.is_forbidden(),
            "a connection failure is not a permission problem"
        );
        assert_eq!(unreachable.reason(), Some(METRICS_REQUEST_FAILED));

        assert_eq!(
            MetricsProbeState::forbidden("list pods.metrics.k8s.io"),
            MetricsProbeState::from_result(Err(forbidden_copy("list pods.metrics.k8s.io"))),
            "the constructor and the error mapping must agree"
        );
    }

    #[test]
    fn denial_copy_hides_the_api_text() {
        let copy = forbidden_copy("list nodes.metrics.k8s.io");
        for internal in ["403", "Status", "ApiError", "stderr", "metrics_k8s_client"] {
            assert!(
                !copy.contains(internal),
                "user-facing copy must not contain {internal:?}: {copy}"
            );
        }
        assert_eq!(
            copy,
            MetricsProbeState::forbidden("list nodes.metrics.k8s.io")
                .reason()
                .expect("a denial explains itself")
        );
    }

    /// The toolbar has to show this app's sampling rate next to the
    /// metrics-server window, or the window reads as the refresh rate.
    #[test]
    fn sampling_cadence_is_copied_apart_from_the_server_window() {
        assert_eq!(
            sampling_cadence_text(Duration::from_secs(10)),
            "Sampling every 10s"
        );
        assert_eq!(
            sampling_cadence_text(Duration::from_secs(30)),
            "Sampling every 30s"
        );
    }

    #[test]
    fn request_errors_are_user_facing() {
        assert_eq!(map_error(MetricsError::Unavailable), METRICS_UNAVAILABLE);

        let rendered = map_error(MetricsError::Quantity {
            value: "??".to_owned(),
        });
        assert_eq!(rendered, METRICS_REQUEST_FAILED);
        assert!(
            rendered.contains("Retry"),
            "User-facing copy must include a next step: {rendered}"
        );
        for internal in [
            "stderr",
            "source",
            "MetricsError",
            "Quantity",
            "parse error",
            "??",
        ] {
            assert!(
                !rendered.contains(internal),
                "User-facing copy must not contain internal field {internal:?}: {rendered}"
            );
        }
    }

    #[test]
    fn capability_reason_stays_user_facing() {
        let state =
            MetricsProbeState::from_result(Err("connection refused: stderr=secret".to_owned()));
        assert_eq!(state.reason(), Some(METRICS_REQUEST_FAILED));
        assert!(!state.reason().unwrap().contains("stderr"));
        assert!(!state.reason().unwrap().contains("secret"));
    }

    #[test]
    fn scheduler_switches_back_to_local_interval() {
        let mut scheduler = MetricsSamples::scheduler_for_tier(LatencyTier::Local);
        assert_eq!(
            scheduler.decide(Duration::ZERO),
            k8s_core::metrics::SampleDecision::SampleNow
        );
        scheduler.set_tier(LatencyTier::HighLatency);
        assert_eq!(
            scheduler.decide(Duration::ZERO),
            k8s_core::metrics::SampleDecision::Wait(Duration::from_secs(30))
        );
        scheduler.set_tier(LatencyTier::Local);
        assert_eq!(
            scheduler.decide(Duration::ZERO),
            k8s_core::metrics::SampleDecision::Wait(Duration::from_secs(10))
        );
    }

    #[test]
    fn retry_copy_matches_scheduler_delay_after_failure_hide_and_show() {
        let mut scheduler = MetricsSamples::scheduler_for_tier(LatencyTier::Local);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
        assert_eq!(retry_delay_text(&scheduler), None);

        scheduler.on_failure();
        let delay = match scheduler.decide(Duration::ZERO) {
            SampleDecision::Wait(delay) => delay,
            decision => panic!("expected a retry delay, got {decision:?}"),
        };
        assert_eq!(delay, Duration::from_secs(11));
        assert_eq!(
            retry_delay_text(&scheduler).as_deref(),
            Some("Retrying in 11 seconds")
        );

        scheduler.set_visible(false);
        assert_eq!(retry_delay_text(&scheduler), None);
        scheduler.set_visible(true);
        assert_eq!(retry_delay_text(&scheduler), None);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
    }

    #[test]
    fn retry_copy_uses_the_current_sampling_interval() {
        let mut scheduler = MetricsSamples::scheduler_for_tier(LatencyTier::HighLatency);
        assert_eq!(scheduler.decide(Duration::ZERO), SampleDecision::SampleNow);
        scheduler.on_failure();
        assert_eq!(
            scheduler.decide(Duration::ZERO),
            SampleDecision::Wait(Duration::from_secs(31))
        );
        assert_eq!(
            retry_delay_text(&scheduler).as_deref(),
            Some("Retrying in 31 seconds")
        );
        assert_eq!(
            sampling_interval_text(scheduler.interval()),
            "New samples arrive every 30 seconds."
        );
    }

    #[test]
    fn target_only_covers_node_and_pod() {
        assert_eq!(
            MetricsTarget::from_object("Node", None, "n1", "uid-1"),
            Some(MetricsTarget::Node {
                name: "n1".to_owned(),
                uid: "uid-1".to_owned()
            })
        );
        assert!(matches!(
            MetricsTarget::from_object("Pod", Some("ns"), "p1", "uid-2"),
            Some(MetricsTarget::Pod { .. })
        ));
        assert!(MetricsTarget::from_object("Service", None, "svc", "uid-3").is_none());
    }

    #[test]
    fn recreated_object_is_a_different_target() {
        let before = MetricsTarget::from_object("Pod", Some("ns"), "web", "uid-old").unwrap();
        let after = MetricsTarget::from_object("Pod", Some("ns"), "web", "uid-new").unwrap();
        assert_ne!(
            before, after,
            "a pod recreated under the same name must restart the series"
        );
        assert_eq!(after.uid(), "uid-new");
    }

    #[test]
    fn timestamp_fallback_uses_now() {
        let metric = NodeMetric {
            name: "n1".to_owned(),
            timestamp: "not a time".to_owned(),
            window: "10s".to_owned(),
            cpu_millicores: Some(1.0),
            memory_bytes: None,
        };
        let payload = SamplePayload::from_node(metric);
        assert!(payload.at_ms > 0);
        assert!(parse_timestamp_ms("2026-09-23T10:00:00Z").is_some());
    }
}
