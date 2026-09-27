//! Aggregates cluster health, workload status, and node capacity.

use std::collections::HashMap;
use std::sync::Arc;

use kube::core::DynamicObject;
use serde_json::Value;

use crate::metrics::{NodeMetric, parse_quantity};

#[derive(Clone, Copy, Debug)]
pub struct ObjectSet<'a> {
    objects: &'a [Arc<DynamicObject>],
    unavailable: Option<&'a str>,
}

impl<'a> Default for ObjectSet<'a> {
    fn default() -> Self {
        Self {
            objects: &[],
            unavailable: Some(SOURCE_NOT_LOADED),
        }
    }
}

impl<'a> ObjectSet<'a> {
    pub fn objects(objects: &'a [Arc<DynamicObject>]) -> Self {
        Self {
            objects,
            unavailable: None,
        }
    }

    pub fn unavailable(reason: &'a str) -> Self {
        Self {
            objects: &[],
            unavailable: Some(reason),
        }
    }

    fn iter(&self) -> std::slice::Iter<'a, Arc<DynamicObject>> {
        self.objects.iter()
    }

    fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn unavailable_reason(&self) -> Option<&'a str> {
        self.unavailable
    }
}

/// Health level from error to healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthLevel {
    Healthy,
    Warning,
    Error,
}

/// Pod counts by health state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HealthSummary {
    pub total_pods: usize,
    pub running: usize,
    pub pending: usize,
    pub failed: usize,
    pub unknown: usize,
}

/// Replica totals for one workload kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplicaSummary {
    pub desired: i64,
    pub available: i64,
}

/// Replica totals for the five workload kinds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkloadCounts {
    pub deployments: ReplicaSummary,
    pub stateful_sets: ReplicaSummary,
    pub daemon_sets: ReplicaSummary,
    pub jobs: ReplicaSummary,
    pub cron_jobs: ReplicaSummary,
}

impl WorkloadCounts {
    /// Replica totals summed over the five workload kinds.
    ///
    /// A dashboard needs one number to answer "are the workloads up", so the
    /// per-kind breakdown stays available next to this total.
    pub fn replicas(&self) -> ReplicaSummary {
        [
            self.deployments,
            self.stateful_sets,
            self.daemon_sets,
            self.jobs,
            self.cron_jobs,
        ]
        .into_iter()
        .fold(ReplicaSummary::default(), |mut total, one| {
            total.desired += one.desired;
            total.available += one.available;
            total
        })
    }

    /// Number of workload kinds the cluster reported at least one replica for.
    pub fn reported_kinds(&self) -> usize {
        [
            self.deployments,
            self.stateful_sets,
            self.daemon_sets,
            self.jobs,
            self.cron_jobs,
        ]
        .into_iter()
        .filter(|summary| summary.desired != 0)
        .count()
    }
}

/// Node readiness counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeSummary {
    pub count: usize,
    pub ready: usize,
    pub not_ready: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnavailableSource {
    pub source: &'static str,
    pub reason: String,
}

/// Why one source is missing from the snapshot.
///
/// A denied source is a different problem from an unreachable cluster, and the
/// UI must not tell a user to check the connection when the cluster answered
/// with "forbidden".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    /// The cluster refused the request: the credentials lack a permission.
    Forbidden,
    /// The cluster could not be reached, or the request timed out.
    Unreachable,
    /// The source was never loaded, so no request was made.
    NotLoaded,
    /// The request failed for another reason.
    Other,
}

/// Source names that carry no data yet because nothing asked for them.
pub const SOURCE_NOT_LOADED: &str = "source not loaded";

/// Number of sources one overview snapshot requests.
pub const OVERVIEW_SOURCE_COUNT: usize = 7;

impl UnavailableSource {
    /// Classify the failure this source reports.
    pub fn failure(&self) -> SourceFailure {
        if self.reason == SOURCE_NOT_LOADED {
            return SourceFailure::NotLoaded;
        }
        let reason = self.reason.to_lowercase();
        // The API server answers an RBAC denial with a 403, and kubectl prints
        // the same code. Both spellings are matched so the state does not
        // depend on which layer produced the text.
        if reason.contains("403") || reason.contains("forbidden") {
            SourceFailure::Forbidden
        } else if reason.contains("timed out")
            || reason.contains("timeout")
            || reason.contains("connection")
            || reason.contains("dns")
        {
            SourceFailure::Unreachable
        } else {
            SourceFailure::Other
        }
    }

    /// True when the cluster denied the request instead of failing to answer.
    pub fn is_forbidden(&self) -> bool {
        self.failure() == SourceFailure::Forbidden
    }

    /// The RBAC permission this source needs, such as `list deployments.apps`.
    ///
    /// `None` when the source is not denied, or when the name is unknown.
    pub fn required_permission(&self) -> Option<&'static str> {
        self.is_forbidden()
            .then(|| required_permission(self.source))
            .flatten()
    }
}

/// The RBAC permission a denied overview source needs.
///
/// The resource is written the way a `Role` names it, group included, so the
/// copy can be pasted into a role or a `kubectl auth can-i` command.
pub fn required_permission(source: &str) -> Option<&'static str> {
    Some(match source {
        "pods" => "list pods",
        "nodes" => "list nodes",
        "deployments" => "list deployments.apps",
        "statefulsets" => "list statefulsets.apps",
        "daemonsets" => "list daemonsets.apps",
        "jobs" => "list jobs.batch",
        "cronjobs" => "list cronjobs.batch",
        _ => return None,
    })
}

/// Node capacity and Pod requests for one node.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeCapacity {
    pub name: String,
    pub allocatable_cpu: Option<f64>,
    pub allocatable_memory: Option<f64>,
    pub requested_cpu: f64,
    pub requested_memory: f64,
    pub limits_cpu: f64,
    pub limits_memory: f64,
    /// `max(requested_cpu/allocatable_cpu, requested_memory/allocatable_memory) * 100`.
    /// `None` means allocatable is unknown or zero. Values above 100 indicate overcommit.
    pub utilization_pct: Option<f64>,
}

/// Node usage when metrics are ready.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeUsage {
    pub name: String,
    pub cpu_millicores: Option<f64>,
    pub memory_bytes: Option<f64>,
}

/// Input object sets for the aggregate.
#[derive(Clone, Copy, Debug, Default)]
pub struct OverviewInput<'a> {
    pub pods: ObjectSet<'a>,
    pub nodes: ObjectSet<'a>,
    pub deployments: ObjectSet<'a>,
    pub stateful_sets: ObjectSet<'a>,
    pub daemon_sets: ObjectSet<'a>,
    pub jobs: ObjectSet<'a>,
    pub cron_jobs: ObjectSet<'a>,
}

/// Complete output from one aggregation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub health: HealthSummary,
    pub workloads: WorkloadCounts,
    pub nodes: NodeSummary,
    pub capacities: Vec<NodeCapacity>,
    /// `None` means metrics are not ready. `Some` means the source responded.
    pub usage: Option<Vec<NodeUsage>>,
    pub has_data: bool,
    pub unknown_pods: usize,
    pub unavailable_workloads: usize,
    pub unavailable_sources: Vec<UnavailableSource>,
}

impl Overview {
    pub fn level(&self) -> HealthLevel {
        let no_data = self.nodes.count == 0
            && self.health.total_pods == 0
            && self.workloads.deployments.desired == 0
            && self.workloads.stateful_sets.desired == 0
            && self.workloads.daemon_sets.desired == 0
            && self.workloads.jobs.desired == 0
            && self.workloads.cron_jobs.desired == 0
            && self.capacities.is_empty();
        let partial = self.nodes.count == 0 || self.capacities.len() != self.nodes.count;
        if self.health.failed > 0 {
            HealthLevel::Error
        } else if self.health.pending > 0
            || self.unknown_pods > 0
            || self.nodes.not_ready > 0
            || self.unavailable_workloads > 0
            || !self.unavailable_sources.is_empty()
            || partial
            || no_data
        {
            HealthLevel::Warning
        } else {
            HealthLevel::Healthy
        }
    }

    pub fn is_complete(&self) -> bool {
        self.unavailable_sources.is_empty()
            && self.nodes.count > 0
            && self.capacities.len() == self.nodes.count
    }

    /// Return true when metrics data is available.
    pub fn metrics_ready(&self) -> bool {
        self.usage.is_some()
    }

    /// Sources the cluster denied, with the permission each one needs.
    pub fn denied_sources(&self) -> impl Iterator<Item = (&UnavailableSource, &'static str)> {
        self.unavailable_sources
            .iter()
            .filter_map(|source| Some((source, source.required_permission()?)))
    }

    /// True when at least one source was denied.
    ///
    /// A denial is a configuration problem, not a cluster problem, so the UI
    /// must not answer it with "check the cluster connection".
    pub fn access_denied(&self) -> bool {
        self.unavailable_sources
            .iter()
            .any(UnavailableSource::is_forbidden)
    }

    /// True when every requested source was denied, so nothing can be shown.
    pub fn fully_denied(&self) -> bool {
        let denied = self
            .unavailable_sources
            .iter()
            .filter(|source| source.is_forbidden())
            .count();
        let other = self
            .unavailable_sources
            .iter()
            .filter(|source| !source.is_forbidden() && source.failure() != SourceFailure::NotLoaded)
            .count();
        denied >= OVERVIEW_SOURCE_COUNT && other == 0
    }
}

/// Aggregate all input. `metrics` is `None` when the source is not ready.
pub fn build(input: &OverviewInput<'_>, metrics: Option<&[NodeMetric]>) -> Overview {
    Overview {
        health: health_summary(input.pods),
        workloads: workload_counts(input),
        nodes: node_summary(input.nodes),
        capacities: node_capacities(input.nodes, input.pods),
        usage: metrics.map(node_usage),
        has_data: has_overview_data(input),
        unknown_pods: unknown_pod_count(input.pods),
        unavailable_workloads: unavailable_workload_count(input),
        unavailable_sources: unavailable_sources(input),
    }
}

/// Count Pods by phase.
fn health_summary(pods: ObjectSet<'_>) -> HealthSummary {
    let mut summary = HealthSummary::default();
    for pod in pods.iter() {
        summary.total_pods += 1;
        match text_at(pod, "/status/phase") {
            Some("Running") => summary.running += 1,
            Some("Pending") => summary.pending += 1,
            Some("Failed") => summary.failed += 1,
            _ => summary.unknown += 1,
        }
    }
    summary
}

/// Replica totals for the five workload kinds.
fn workload_counts(input: &OverviewInput<'_>) -> WorkloadCounts {
    WorkloadCounts {
        deployments: sum_workloads(input.deployments, deployment_replicas),
        stateful_sets: sum_workloads(input.stateful_sets, stateful_set_replicas),
        daemon_sets: sum_workloads(input.daemon_sets, daemon_set_replicas),
        jobs: sum_workloads(input.jobs, job_replicas),
        cron_jobs: sum_workloads(input.cron_jobs, cron_job_replicas),
    }
}

fn has_overview_data(input: &OverviewInput<'_>) -> bool {
    !input.pods.is_empty()
        || !input.nodes.is_empty()
        || !input.deployments.is_empty()
        || !input.stateful_sets.is_empty()
        || !input.daemon_sets.is_empty()
        || !input.jobs.is_empty()
        || !input.cron_jobs.is_empty()
}

fn unknown_pod_count(pods: ObjectSet<'_>) -> usize {
    pods.iter()
        .filter(|pod| {
            let pod: &DynamicObject = pod;
            !matches!(
                text_at(pod, "/status/phase"),
                Some("Running" | "Pending" | "Failed" | "Succeeded")
            )
        })
        .count()
}

fn unavailable_workload_count(input: &OverviewInput<'_>) -> usize {
    let standard = [
        (
            input.deployments,
            deployment_replicas as fn(&DynamicObject) -> ReplicaSummary,
        ),
        (
            input.stateful_sets,
            stateful_set_replicas as fn(&DynamicObject) -> ReplicaSummary,
        ),
        (
            input.daemon_sets,
            daemon_set_replicas as fn(&DynamicObject) -> ReplicaSummary,
        ),
        (
            input.jobs,
            job_replicas as fn(&DynamicObject) -> ReplicaSummary,
        ),
    ]
    .into_iter()
    .map(|(source, project)| {
        source
            .iter()
            .filter(|object| {
                let object: &DynamicObject = object;
                let replicas = project(object);
                replicas.desired > replicas.available
            })
            .count()
    })
    .sum::<usize>();
    let cron_jobs = input
        .cron_jobs
        .iter()
        .filter(|object| {
            let object: &DynamicObject = object;
            let suspended = object
                .data
                .pointer("/spec/suspend")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let replicas = cron_job_replicas(object);
            !suspended && replicas.desired > replicas.available
        })
        .count();
    standard + cron_jobs
}

fn unavailable_sources(input: &OverviewInput<'_>) -> Vec<UnavailableSource> {
    [
        ("pods", input.pods),
        ("nodes", input.nodes),
        ("deployments", input.deployments),
        ("statefulsets", input.stateful_sets),
        ("daemonsets", input.daemon_sets),
        ("jobs", input.jobs),
        ("cronjobs", input.cron_jobs),
    ]
    .into_iter()
    .filter_map(|(source, objects)| {
        objects
            .unavailable_reason()
            .map(|reason| UnavailableSource {
                source,
                reason: reason.to_owned(),
            })
    })
    .collect()
}

/// Count ready nodes from `status.conditions`.
fn node_summary(nodes: ObjectSet<'_>) -> NodeSummary {
    let mut summary = NodeSummary::default();
    for node in nodes.iter() {
        summary.count += 1;
        if node_is_ready(node) {
            summary.ready += 1;
        } else {
            summary.not_ready += 1;
        }
    }
    summary
}

/// Aggregate node capacity from allocatable values and Pod requests.
fn node_capacities(nodes: ObjectSet<'_>, pods: ObjectSet<'_>) -> Vec<NodeCapacity> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut capacities: Vec<NodeCapacity> = Vec::new();

    for node in nodes.iter() {
        let Some(name) = node.metadata.name.clone() else {
            continue;
        };
        if index.contains_key(&name) {
            continue;
        }
        index.insert(name.clone(), capacities.len());
        capacities.push(NodeCapacity {
            name,
            allocatable_cpu: quantity_at(node, "/status/allocatable/cpu"),
            allocatable_memory: quantity_at(node, "/status/allocatable/memory"),
            requested_cpu: 0.0,
            requested_memory: 0.0,
            limits_cpu: 0.0,
            limits_memory: 0.0,
            utilization_pct: None,
        });
    }

    for pod in pods.iter() {
        let Some(node_name) = text_at(pod, "/spec/nodeName") else {
            continue;
        };
        let Some(&position) = index.get(node_name) else {
            continue;
        };
        let (requested_cpu, requested_memory) = pod_resources(pod, "requests");
        let (limits_cpu, limits_memory) = pod_resources(pod, "limits");
        let capacity = &mut capacities[position];
        capacity.requested_cpu += requested_cpu;
        capacity.requested_memory += requested_memory;
        capacity.limits_cpu += limits_cpu;
        capacity.limits_memory += limits_memory;
    }

    for capacity in &mut capacities {
        capacity.utilization_pct = utilization_pct(capacity);
    }
    capacities.sort_by(|a, b| a.name.cmp(&b.name));
    capacities
}

/// Convert metrics samples to per-node usage.
fn node_usage(metrics: &[NodeMetric]) -> Vec<NodeUsage> {
    metrics
        .iter()
        .map(|metric| NodeUsage {
            name: metric.name.clone(),
            cpu_millicores: metric.cpu_millicores,
            memory_bytes: metric.memory_bytes,
        })
        .collect()
}

fn sum_workloads(
    source: ObjectSet<'_>,
    project: fn(&DynamicObject) -> ReplicaSummary,
) -> ReplicaSummary {
    source
        .iter()
        .fold(ReplicaSummary::default(), |mut total, obj| {
            let one = project(obj);
            total.desired += one.desired;
            total.available += one.available;
            total
        })
}

fn deployment_replicas(obj: &DynamicObject) -> ReplicaSummary {
    ReplicaSummary {
        desired: int_at(obj, "/spec/replicas").unwrap_or(1).max(0),
        available: int_at(obj, "/status/availableReplicas").unwrap_or(0).max(0),
    }
}

fn stateful_set_replicas(obj: &DynamicObject) -> ReplicaSummary {
    ReplicaSummary {
        desired: int_at(obj, "/spec/replicas").unwrap_or(1).max(0),
        available: int_at(obj, "/status/availableReplicas")
            .or_else(|| int_at(obj, "/status/readyReplicas"))
            .unwrap_or(0)
            .max(0),
    }
}

fn daemon_set_replicas(obj: &DynamicObject) -> ReplicaSummary {
    ReplicaSummary {
        desired: int_at(obj, "/status/desiredNumberScheduled")
            .unwrap_or(0)
            .max(0),
        available: int_at(obj, "/status/numberAvailable")
            .or_else(|| int_at(obj, "/status/numberReady"))
            .unwrap_or(0)
            .max(0),
    }
}

fn job_replicas(obj: &DynamicObject) -> ReplicaSummary {
    ReplicaSummary {
        desired: int_at(obj, "/spec/completions").unwrap_or(1).max(0),
        available: int_at(obj, "/status/succeeded").unwrap_or(0).max(0),
    }
}

fn cron_job_replicas(obj: &DynamicObject) -> ReplicaSummary {
    let suspended = obj
        .data
        .pointer("/spec/suspend")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ReplicaSummary {
        desired: 1,
        available: i64::from(!suspended),
    }
}

fn node_is_ready(node: &DynamicObject) -> bool {
    let mut found = false;
    let mut ready = true;
    for condition in node
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if text_at(condition, "/type") == Some("Ready") {
            found = true;
            ready &= text_at(condition, "/status") == Some("True");
        }
    }
    found && ready
}

/// Sum one Pod's CPU and memory resources for a request section.
fn pod_resources(pod: &DynamicObject, section: &str) -> (f64, f64) {
    let mut cpu = 0.0;
    let mut memory = 0.0;

    if let Some(containers) = pod
        .data
        .pointer("/spec/containers")
        .and_then(Value::as_array)
    {
        for container in containers {
            cpu += container_quantity(container, section, "cpu");
            memory += container_quantity(container, section, "memory");
        }
    }

    if let Some(init_containers) = pod
        .data
        .pointer("/spec/initContainers")
        .and_then(Value::as_array)
    {
        let init_cpu = init_containers
            .iter()
            .map(|container| container_quantity(container, section, "cpu"))
            .fold(0.0_f64, f64::max);
        let init_memory = init_containers
            .iter()
            .map(|container| container_quantity(container, section, "memory"))
            .fold(0.0_f64, f64::max);
        cpu = cpu.max(init_cpu);
        memory = memory.max(init_memory);
    }

    if let Some(overhead) = pod.data.pointer("/spec/overhead") {
        cpu += value_quantity(overhead, "/cpu");
        memory += value_quantity(overhead, "/memory");
    }

    (cpu, memory)
}

fn container_quantity(container: &Value, section: &str, resource: &str) -> f64 {
    value_quantity(container, &format!("/resources/{section}/{resource}"))
}

fn utilization_pct(capacity: &NodeCapacity) -> Option<f64> {
    let cpu = capacity
        .allocatable_cpu
        .filter(|allocatable| *allocatable > 0.0)
        .map(|allocatable| capacity.requested_cpu / allocatable);
    let memory = capacity
        .allocatable_memory
        .filter(|allocatable| *allocatable > 0.0)
        .map(|allocatable| capacity.requested_memory / allocatable);
    cpu.into_iter()
        .chain(memory)
        .reduce(f64::max)
        .map(|ratio| ratio * 100.0)
}

fn text_at<'a>(root: &'a impl PointerRoot, pointer: &str) -> Option<&'a str> {
    root.pointer(pointer).and_then(Value::as_str)
}

fn int_at(root: &impl PointerRoot, pointer: &str) -> Option<i64> {
    root.pointer(pointer).and_then(Value::as_i64)
}

fn quantity_at(root: &impl PointerRoot, pointer: &str) -> Option<f64> {
    text_at(root, pointer).and_then(|raw| parse_quantity(raw).ok())
}

fn value_quantity(root: &Value, pointer: &str) -> f64 {
    text_at(root, pointer)
        .and_then(|raw| parse_quantity(raw).ok())
        .unwrap_or(0.0)
}

/// Support JSON Pointer access for dynamic objects and JSON values.
trait PointerRoot {
    fn pointer(&self, pointer: &str) -> Option<&Value>;
}

impl PointerRoot for DynamicObject {
    fn pointer(&self, pointer: &str) -> Option<&Value> {
        self.data.pointer(pointer)
    }
}

impl PointerRoot for Arc<DynamicObject> {
    fn pointer(&self, pointer: &str) -> Option<&Value> {
        self.data.pointer(pointer)
    }
}

impl PointerRoot for Value {
    fn pointer(&self, pointer: &str) -> Option<&Value> {
        Value::pointer(self, pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Arc<DynamicObject> {
        Arc::new(serde_json::from_value(value).expect("synthetic DynamicObject"))
    }

    fn pod(name: &str, node: Option<&str>, phase: Option<&str>) -> Arc<DynamicObject> {
        let mut value = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "namespace": "ns" },
            "spec": {},
        });
        if let Some(node) = node {
            value["spec"]["nodeName"] = Value::String(node.to_string());
        }
        if let Some(phase) = phase {
            value["status"] = serde_json::json!({ "phase": phase });
        }
        object(value)
    }

    fn node(
        name: &str,
        ready: Option<bool>,
        allocatable: Option<(&str, &str)>,
    ) -> Arc<DynamicObject> {
        let mut status = serde_json::Map::new();
        if let Some(ready) = ready {
            status.insert(
                "conditions".to_string(),
                serde_json::json!([{
                    "type": "Ready",
                    "status": if ready { "True" } else { "False" },
                }]),
            );
        }
        if let Some((cpu, memory)) = allocatable {
            status.insert(
                "allocatable".to_string(),
                serde_json::json!({ "cpu": cpu, "memory": memory }),
            );
        }
        object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": { "name": name },
            "status": status,
        }))
    }

    fn resource_pod(name: &str, node: &str, resources: Value) -> Arc<DynamicObject> {
        object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name },
            "spec": {
                "nodeName": node,
                "containers": [{ "name": "app", "resources": resources }],
            },
        }))
    }

    fn node_metric(name: &str, cpu: Option<f64>, memory: Option<f64>) -> NodeMetric {
        NodeMetric {
            name: name.to_string(),
            timestamp: "2026-09-24T00:00:00Z".to_string(),
            window: "10s".to_string(),
            cpu_millicores: cpu,
            memory_bytes: memory,
        }
    }

    fn loaded_input<'a>(
        pods: &'a [Arc<DynamicObject>],
        nodes: &'a [Arc<DynamicObject>],
    ) -> OverviewInput<'a> {
        let empty: &'a [Arc<DynamicObject>] = &[];
        OverviewInput {
            pods: ObjectSet::objects(pods),
            nodes: ObjectSet::objects(nodes),
            deployments: ObjectSet::objects(empty),
            stateful_sets: ObjectSet::objects(empty),
            daemon_sets: ObjectSet::objects(empty),
            jobs: ObjectSet::objects(empty),
            cron_jobs: ObjectSet::objects(empty),
        }
    }

    fn overview_for(pods: &[Arc<DynamicObject>], nodes: &[Arc<DynamicObject>]) -> Overview {
        build(&loaded_input(pods, nodes), None)
    }

    #[test]
    fn empty_or_partial_input_is_not_healthy() {
        assert_eq!(Overview::default().level(), HealthLevel::Warning);
        let missing = build(&OverviewInput::default(), None);
        assert_eq!(missing.health, HealthSummary::default());
        assert_eq!(missing.nodes, NodeSummary::default());
        assert_eq!(missing.workloads, WorkloadCounts::default());
        assert!(missing.capacities.is_empty());
        assert!(!missing.metrics_ready());
        assert!(!missing.is_complete());
        assert_eq!(missing.unavailable_sources.len(), 7);
        assert!(
            missing
                .unavailable_sources
                .iter()
                .all(|source| source.reason == SOURCE_NOT_LOADED)
        );
        assert_eq!(missing.level(), HealthLevel::Warning);

        let empty = build(&loaded_input(&[], &[]), None);
        assert!(!empty.is_complete());
        assert!(empty.unavailable_sources.is_empty());
        assert_eq!(empty.level(), HealthLevel::Warning);
    }

    #[test]
    fn pod_health_buckets_partition_total() {
        let pods = vec![
            pod("running", None, Some("Running")),
            pod("pending", None, Some("Pending")),
            pod("failed", None, Some("Failed")),
            pod("succeeded", None, Some("Succeeded")),
            pod("unknown", None, Some("Unknown")),
            pod("missing", None, None),
        ];
        let health = health_summary(ObjectSet::objects(&pods));

        assert_eq!(health.total_pods, 6);
        assert_eq!(health.running, 1);
        assert_eq!(health.pending, 1);
        assert_eq!(health.failed, 1);
        assert_eq!(
            health.unknown, 3,
            "Succeeded, Unknown, and missing phases count as unknown"
        );
        assert_eq!(
            health.running + health.pending + health.failed + health.unknown,
            health.total_pods
        );
    }

    #[test]
    fn banner_level_reflects_pod_node_and_data_state() {
        let running = vec![pod("a", None, Some("Running"))];
        let ready_node = vec![node("n", Some(true), None)];
        assert_eq!(
            overview_for(&running, &ready_node).level(),
            HealthLevel::Healthy
        );

        let pending = vec![pod("a", None, Some("Pending"))];
        assert_eq!(
            overview_for(&pending, &ready_node).level(),
            HealthLevel::Warning
        );

        let unknown = vec![pod("a", None, Some("Unknown"))];
        let unknown = overview_for(&unknown, &ready_node);
        assert_eq!(unknown.unknown_pods, 1);
        assert_eq!(unknown.level(), HealthLevel::Warning);

        let succeeded = vec![pod("a", None, Some("Succeeded"))];
        let succeeded = overview_for(&succeeded, &ready_node);
        assert_eq!(succeeded.health.unknown, 1);
        assert_eq!(succeeded.unknown_pods, 0);
        assert_eq!(succeeded.level(), HealthLevel::Healthy);

        let not_ready = vec![node("n", Some(false), None)];
        assert_eq!(
            overview_for(&running, &not_ready).level(),
            HealthLevel::Warning
        );

        let failed = vec![pod("a", None, Some("Failed"))];
        assert_eq!(
            overview_for(&failed, &not_ready).level(),
            HealthLevel::Error
        );

        let mut partial = loaded_input(&running, &ready_node);
        partial.deployments = ObjectSet::unavailable("forbidden");
        let partial = build(&partial, None);
        assert!(!partial.is_complete());
        assert_eq!(partial.level(), HealthLevel::Warning);
        assert_eq!(partial.unavailable_sources[0].source, "deployments");
        assert_eq!(partial.unavailable_sources[0].reason, "forbidden");
    }

    #[test]
    fn node_summary_counts_ready_conditions() {
        let nodes = vec![
            node("ready", Some(true), None),
            node("not-ready", Some(false), None),
            node("no-conditions", None, None),
            object(serde_json::json!({
                "metadata": { "name": "unknown-ready" },
                "status": { "conditions": [{ "type": "Ready", "status": "Unknown" }] },
            })),
            object(serde_json::json!({
                "metadata": { "name": "unrelated-only" },
                "status": { "conditions": [{ "type": "MemoryPressure", "status": "True" }] },
            })),
            object(serde_json::json!({
                "metadata": { "name": "conflicting-ready" },
                "status": { "conditions": [
                    { "type": "Ready", "status": "True" },
                    { "type": "Ready", "status": "False" },
                ] },
            })),
        ];
        assert_eq!(
            node_summary(ObjectSet::objects(&nodes)),
            NodeSummary {
                count: 6,
                ready: 1,
                not_ready: 5,
            }
        );
    }

    #[test]
    fn workload_counts_sum_desired_and_available() {
        let deployments = vec![
            object(serde_json::json!({
                "spec": { "replicas": 3 },
                "status": { "availableReplicas": 2 },
            })),
            object(serde_json::json!({})),
        ];
        let stateful_sets = vec![object(serde_json::json!({
            "spec": { "replicas": 2 },
            "status": { "readyReplicas": 2 },
        }))];
        let daemon_sets = vec![object(serde_json::json!({
            "status": { "desiredNumberScheduled": 3, "numberAvailable": 2 },
        }))];
        let jobs = vec![
            object(serde_json::json!({ "status": { "succeeded": 1 } })),
            object(serde_json::json!({
                "spec": { "completions": 4 },
                "status": { "succeeded": 3 },
            })),
        ];
        let cron_jobs = vec![
            object(serde_json::json!({ "spec": {} })),
            object(serde_json::json!({ "spec": { "suspend": true } })),
        ];
        let input = OverviewInput {
            deployments: ObjectSet::objects(&deployments),
            stateful_sets: ObjectSet::objects(&stateful_sets),
            daemon_sets: ObjectSet::objects(&daemon_sets),
            jobs: ObjectSet::objects(&jobs),
            cron_jobs: ObjectSet::objects(&cron_jobs),
            ..OverviewInput::default()
        };
        let counts = workload_counts(&input);

        assert_eq!(
            counts.deployments,
            ReplicaSummary {
                desired: 4,
                available: 2,
            }
        );
        assert_eq!(
            counts.stateful_sets,
            ReplicaSummary {
                desired: 2,
                available: 2,
            }
        );
        assert_eq!(
            counts.daemon_sets,
            ReplicaSummary {
                desired: 3,
                available: 2,
            }
        );
        assert_eq!(
            counts.jobs,
            ReplicaSummary {
                desired: 5,
                available: 4,
            }
        );
        assert_eq!(
            counts.cron_jobs,
            ReplicaSummary {
                desired: 2,
                available: 1,
            }
        );
    }

    #[test]
    fn unavailable_workloads_lower_health_without_counting_completed_or_suspended_work() {
        let pods = vec![pod("running", None, Some("Running"))];
        let nodes = vec![node("ready", Some(true), None)];
        let deployments = vec![object(serde_json::json!({
            "spec": { "replicas": 3 },
            "status": { "availableReplicas": 2 },
        }))];
        let jobs = vec![object(serde_json::json!({
            "spec": { "completions": 1 },
            "status": { "succeeded": 1 },
        }))];
        let cron_jobs = vec![object(serde_json::json!({ "spec": { "suspend": true } }))];
        let mut input = loaded_input(&pods, &nodes);
        input.deployments = ObjectSet::objects(&deployments);
        input.jobs = ObjectSet::objects(&jobs);
        input.cron_jobs = ObjectSet::objects(&cron_jobs);

        let overview = build(&input, None);
        assert_eq!(overview.unavailable_workloads, 1);
        assert_eq!(overview.level(), HealthLevel::Warning);
    }

    #[test]
    fn node_capacity_sums_requests_limits_and_skips_foreign_pods() {
        let nodes = vec![
            node("worker-b", Some(true), Some(("2000m", "1Gi"))),
            node("worker-a", Some(true), Some(("4", "8Gi"))),
        ];
        let pods = vec![
            resource_pod(
                "p1",
                "worker-a",
                serde_json::json!({
                    "requests": { "cpu": "500m", "memory": "256Mi" },
                    "limits": { "cpu": "1", "memory": "512Mi" },
                }),
            ),
            object(serde_json::json!({
                "metadata": { "name": "p2" },
                "spec": {
                    "nodeName": "worker-a",
                    "containers": [{
                        "name": "app",
                        "resources": { "requests": { "cpu": "100m" } },
                    }],
                    "initContainers": [{
                        "name": "init",
                        "resources": {
                            "requests": { "cpu": "2", "memory": "1Gi" },
                            "limits": { "cpu": "3" },
                        },
                    }],
                    "overhead": { "cpu": "250m", "memory": "100Mi" },
                },
            })),
            object(serde_json::json!({
                "metadata": { "name": "unscheduled" },
                "spec": {
                    "containers": [{
                        "name": "app",
                        "resources": { "requests": { "cpu": "10" } },
                    }],
                },
            })),
            object(serde_json::json!({
                "metadata": { "name": "ghost" },
                "spec": {
                    "nodeName": "worker-x",
                    "containers": [{
                        "name": "app",
                        "resources": { "requests": { "cpu": "5" } },
                    }],
                },
            })),
        ];
        let capacities = node_capacities(ObjectSet::objects(&nodes), ObjectSet::objects(&pods));

        assert_eq!(capacities.len(), 2, "nodes are sorted by name");
        let worker_a = &capacities[0];
        assert_eq!(worker_a.name, "worker-a");
        assert_eq!(worker_a.allocatable_cpu, Some(4.0));
        assert_eq!(worker_a.allocatable_memory, Some(8.0 * 1073741824.0));
        assert_eq!(worker_a.requested_cpu, 2.75, "500m + max(100m, 2) + 250m");
        assert_eq!(worker_a.requested_memory, 1380.0 * 1048576.0);
        assert_eq!(worker_a.limits_cpu, 4.25, "1 + max(0, 3) + 250m");
        assert_eq!(worker_a.limits_memory, 612.0 * 1048576.0);
        assert!(
            (worker_a.utilization_pct.expect("allocatable is present") - 68.75).abs() < 1e-9,
            "2.75/4 > 1380Mi/8Gi"
        );

        let worker_b = &capacities[1];
        assert_eq!(worker_b.name, "worker-b");
        assert_eq!(worker_b.allocatable_cpu, Some(2.0));
        assert_eq!(worker_b.requested_cpu, 0.0);
        assert_eq!(worker_b.limits_memory, 0.0);
        assert_eq!(worker_b.utilization_pct, Some(0.0));
    }

    #[test]
    fn missing_fields_yield_none_allocatable_and_zero_totals() {
        let nodes = vec![
            object(serde_json::json!({ "metadata": { "name": "n1" } })),
            object(serde_json::json!({
                "metadata": { "name": "n2" },
                "status": { "allocatable": { "cpu": "bogus", "memory": 5 } },
            })),
            object(serde_json::json!({})),
        ];
        let pods = vec![object(serde_json::json!({
            "metadata": { "name": "p" },
            "spec": {
                "nodeName": "n1",
                "containers": [{
                    "name": "c",
                    "resources": { "requests": { "cpu": "??" } },
                }],
            },
        }))];
        let capacities = node_capacities(ObjectSet::objects(&nodes), ObjectSet::objects(&pods));

        assert_eq!(
            capacities.len(),
            2,
            "nodes without metadata.name cannot be attributed"
        );
        assert_eq!(capacities[0].name, "n1");
        assert_eq!(capacities[0].allocatable_cpu, None);
        assert_eq!(capacities[0].allocatable_memory, None);
        assert_eq!(
            capacities[0].requested_cpu, 0.0,
            "invalid quantity becomes zero"
        );
        assert_eq!(
            capacities[0].utilization_pct, None,
            "unknown allocatable has no utilization value"
        );
        assert_eq!(
            capacities[1].allocatable_cpu, None,
            "a non-string quantity is missing"
        );
        assert_eq!(capacities[1].allocatable_memory, None);
        assert_eq!(node_summary(ObjectSet::objects(&nodes)).count, 3);
        assert!(!build(&loaded_input(&[], &nodes), None).is_complete());
    }

    #[test]
    fn usage_present_only_when_metrics_ready() {
        let metrics = vec![
            node_metric("n1", Some(250.0), Some(1024.0)),
            node_metric("n2", None, None),
        ];
        let ready = build(&OverviewInput::default(), Some(&metrics));
        assert!(ready.metrics_ready());
        let usage = ready.usage.as_ref().expect("metrics are ready");
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].name, "n1");
        assert_eq!(usage[0].cpu_millicores, Some(250.0));
        assert_eq!(usage[1].cpu_millicores, None);

        let empty = build(&OverviewInput::default(), Some(&[]));
        assert!(
            empty.metrics_ready(),
            "a successful probe with no samples keeps the card ready"
        );
        assert!(!build(&OverviewInput::default(), None).metrics_ready());
    }

    #[test]
    fn object_set_uses_loaded_objects_or_an_unavailable_reason() {
        let pods = vec![pod("a", None, Some("Running"))];
        let set = ObjectSet::objects(&pods);

        assert_eq!(set.iter().count(), 1);
        assert_eq!(health_summary(set).running, 1);

        let unavailable = ObjectSet::unavailable("forbidden");
        assert_eq!(unavailable.iter().count(), 0);
        assert_eq!(unavailable.unavailable_reason(), Some("forbidden"));
        assert_eq!(ObjectSet::default().iter().count(), 0);
    }

    fn denied(reason: &str) -> UnavailableSource {
        UnavailableSource {
            source: "deployments",
            reason: reason.to_owned(),
        }
    }

    /// A 403 is a configuration problem. It must not be reported as a
    /// connection failure, and the copy must name the missing permission.
    #[test]
    fn rbac_denial_is_a_state_of_its_own() {
        let from_api = denied(
            "Failed to list deployments: ApiError: deployments.apps is forbidden: \
             User \"dev\" cannot list resource \"deployments\" (Status { code: 403, reason: \
             \"Forbidden\" }). Check the cluster connection and permissions, then try again.",
        );
        assert_eq!(from_api.failure(), SourceFailure::Forbidden);
        assert!(from_api.is_forbidden());
        assert_eq!(
            from_api.required_permission(),
            Some("list deployments.apps")
        );

        let from_kubectl = denied(
            "kubectl deployments failed with HTTP 403: forbidden. Check permissions and try again.",
        );
        assert_eq!(from_kubectl.failure(), SourceFailure::Forbidden);
        assert_eq!(
            from_kubectl.required_permission(),
            Some("list deployments.apps"),
            "the word 'forbidden' alone is enough"
        );

        let timeout = denied("Failed to list deployments: request timed out after 8s.");
        assert_eq!(timeout.failure(), SourceFailure::Unreachable);
        assert!(
            !timeout.is_forbidden(),
            "a timeout is not a permission problem"
        );

        let missing = denied(SOURCE_NOT_LOADED);
        assert_eq!(missing.failure(), SourceFailure::NotLoaded);
        assert_eq!(missing.required_permission(), None);

        let other = denied("Failed to list deployments: parse error.");
        assert_eq!(other.failure(), SourceFailure::Other);
        assert_eq!(other.required_permission(), None);

        assert_eq!(required_permission("cronjobs"), Some("list cronjobs.batch"));
        assert_eq!(required_permission("services"), None);
    }

    #[test]
    fn denial_counting_separates_partial_from_total_denial() {
        let source = |name: &'static str, reason: &str| UnavailableSource {
            source: name,
            reason: reason.to_owned(),
        };
        let all_sources = |reason: &str| {
            [
                "pods",
                "nodes",
                "deployments",
                "statefulsets",
                "daemonsets",
                "jobs",
                "cronjobs",
            ]
            .into_iter()
            .map(|name| source(name, reason))
            .collect::<Vec<_>>()
        };
        let partial = Overview {
            unavailable_sources: vec![source("pods", "403 Forbidden")],
            ..Overview::default()
        };
        assert!(partial.access_denied());
        assert!(
            !partial.fully_denied(),
            "one denied source is partial data, not a blind overview"
        );
        assert_eq!(
            partial
                .denied_sources()
                .map(|(_, permission)| permission)
                .collect::<Vec<_>>(),
            vec!["list pods"]
        );

        let denied_all = Overview {
            unavailable_sources: all_sources("request failed: (Status { code: 403 })"),
            ..Overview::default()
        };
        assert!(denied_all.access_denied());
        assert!(
            denied_all.fully_denied(),
            "every requested source was denied, so nothing is reachable"
        );
        assert!(!denied_all.is_complete());

        let mixed = Overview {
            unavailable_sources: all_sources("request failed: (Status { code: 403 })")
                .into_iter()
                .map(|mut entry| {
                    if entry.source == "pods" {
                        entry.reason = "request timed out after 8s".to_owned();
                    }
                    entry
                })
                .collect(),
            ..Overview::default()
        };
        assert!(
            mixed.access_denied() && !mixed.fully_denied(),
            "a mixed failure is a connection problem, not a clean denial"
        );
    }

    #[test]
    fn workload_replicas_sum_every_kind() {
        let counts = WorkloadCounts {
            deployments: ReplicaSummary {
                desired: 3,
                available: 3,
            },
            stateful_sets: ReplicaSummary {
                desired: 2,
                available: 1,
            },
            daemon_sets: ReplicaSummary {
                desired: 4,
                available: 4,
            },
            jobs: ReplicaSummary {
                desired: 1,
                available: 0,
            },
            cron_jobs: ReplicaSummary::default(),
        };
        assert_eq!(
            counts.replicas(),
            ReplicaSummary {
                desired: 10,
                available: 8
            }
        );
        assert_eq!(counts.reported_kinds(), 4);
        assert_eq!(WorkloadCounts::default().reported_kinds(), 0);
    }
}
