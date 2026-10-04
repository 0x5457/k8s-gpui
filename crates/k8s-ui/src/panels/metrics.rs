//! Metrics sampling and chart data.
//! Sampling runs while the Metrics tab is visible.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::task::join_abortable;
use gpui_kit::SharedString;
use k8s_core::cluster_data::ClusterDataSource;
use k8s_core::latency::{Latency, LatencyTier};
use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric, SampleScheduler, TimeSeries};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::charts::{ChartData, Series, SeriesColor, Unit};
use crate::session::OpsFuture;

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
///
/// It is the second step of the ladder rather than the first, and it is the shortest step that
/// still shows a shape. One minute of a Pod's CPU is a flat line or a spike; fifteen is the
/// shortest window in which "this looks wrong" is something a reader can see.
pub const DEFAULT_RANGE_MS: i64 = 15 * 60 * 1000;

/// Chart and table range options in milliseconds, shortest first.
///
/// `UI-SPEC` §16.5 fixes this ladder and fixes it as a *segmented control*, so the shape of this
/// table is a UI decision: six steps is the most that still reads as one row of equal parts, and
/// the steps are the ones a reader actually asks for — what is happening now, what happened while
/// I was in the incident, what happened yesterday, what happened last week.
///
/// The ladder is also the buffer's retention, because [`METRICS_CAPACITY`] is derived from the
/// longest step. Seven days at the fastest sampling tier is 60,480 samples of `i64` + `f64` per
/// series, so a ten-container Pod holds about 19 MB of samples the chart never draws:
/// `charts::Series::from_time_series` downsamples to `MAX_PLOT_POINTS` before anything is
/// painted. The cost is bounded and it is the price of the step being selectable at all, but a
/// reader on a large cluster should know it is theirs. See the delivery note.
pub const RANGE_OPTIONS: [(i64, &str); 6] = [
    (60 * 1000, "1m"),
    (15 * 60 * 1000, "15m"),
    (60 * 60 * 1000, "1h"),
    (6 * 60 * 60 * 1000, "6h"),
    (24 * 60 * 60 * 1000, "24h"),
    (7 * 24 * 60 * 60 * 1000, "7d"),
];

/// Longest selectable chart range in milliseconds.
pub const MAX_RANGE_MS: i64 = RANGE_OPTIONS[RANGE_OPTIONS.len() - 1].0;

/// What a node that reports nothing means, in a sentence a reader can act on.
///
/// `UI-SPEC` §16.5 asks the empty state to carry the *reason* and not only the fact, and the
/// reason is not knowable from the absence: a Pod that is still Pending and a cluster with
/// nothing reporting both answer with no sample, and the second one is a cluster problem while
/// the first clears on its own.
///
/// These are two sentences rather than a classifier, and the classifier is gone on purpose.
/// `MetricsError` already carries the answer — a 404, a 403, a timeout, an answer with no
/// sample — so the four classes that used to sit beside it were read back out of the English
/// sentence they had just been written into, three word lists deep, and could only ever be as
/// right as the wording. A class that is decided once, from the typed error, cannot drift from
/// the copy that states it.
const METRICS_NOT_REPORTED: &str =
    "No metrics are being reported for this node. Install or enable metrics-server, then retry.";

/// What a Pod with no sample means: it has nothing to report yet.
///
/// Telling this reader to install metrics-server sends them out of the middle of an incident
/// for a Pod that was started ninety seconds ago.
const POD_NOT_RUNNING: &str =
    "The pod is not running, so it has nothing to report. Wait for it to start, then retry.";

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
                // A node with no sample and a cluster with nothing reporting are the same
                // absence, and the one sentence that names it is the one that fixes it.
                METRICS_NOT_REPORTED.to_owned()
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
            // A Pod that is still Pending has no sample and is not a cluster problem. The
            // request itself succeeded, so a surface that reports a Pending Pod as a missing
            // metrics-server sends the reader out of the incident to install something they do
            // not need.
            metric.map(SamplePayload::from_pod).ok_or_else(|| {
                format!("The pod {namespace}/{name} reported no metrics. {POD_NOT_RUNNING}")
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

    pub fn interval_ms(scheduler: &SampleScheduler) -> i64 {
        i64::try_from(scheduler.interval().as_millis()).unwrap_or(i64::MAX)
    }
}

pub fn sampling_interval_text(interval: Duration) -> String {
    format!("New samples arrive every {} seconds.", interval.as_secs())
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

    /// The range ladder is a UI decision (`UI-SPEC` §16.5), so the facts a surface needs to
    /// draw it — that every step is offered, that the default is on the ladder, and that the
    /// buffer covers the longest step — are held here rather than re-derived at each call site.
    #[test]
    fn the_range_ladder_is_the_specified_six_and_the_buffer_covers_it() {
        assert_eq!(
            RANGE_OPTIONS.map(|(_, label)| label),
            ["1m", "15m", "1h", "6h", "24h", "7d"],
            "§16.5 fixes the ladder; a surface that draws fewer steps is drawing the wrong control"
        );
        assert!(RANGE_OPTIONS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(
            RANGE_OPTIONS
                .iter()
                .any(|(millis, label)| *millis == DEFAULT_RANGE_MS && *label == "15m"),
            "the default has to be a step, or the segmented control opens with nothing checked"
        );
        assert_eq!(
            METRICS_CAPACITY as i64 * FASTEST_SAMPLE_INTERVAL_MS,
            MAX_RANGE_MS + FASTEST_SAMPLE_INTERVAL_MS * 2,
            "the buffer has to cover the longest step at the fastest cadence, or picking 7d shows \
             only the samples that happen to still be in memory"
        );
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
}
