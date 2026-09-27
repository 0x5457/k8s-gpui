use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::cluster::{ClusterId, ClusterRegistry};
use crate::latency::{NON_WATCH_READ_TIMEOUT, with_read_timeout};
use crate::metrics::{MetricsError, NodeMetric, PodMetric};
use crate::overview::{ObjectSet, Overview, OverviewInput};
use k8s_openapi::api::core::v1::Namespace;
use kube::Client;
use kube::api::{Api, ApiResource, ListParams};
use kube::core::{DynamicObject, GroupVersionKind};
use tokio::task::JoinSet;
use tokio::time::Instant;

pub type DataFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

pub const OVERVIEW_DEADLINE: Duration = NON_WATCH_READ_TIMEOUT;

type ObjectListResult = Result<Vec<Arc<DynamicObject>>, String>;

enum OverviewTaskResult {
    Objects(&'static str, ObjectListResult),
    Metrics(Result<Vec<NodeMetric>, MetricsError>),
}

pub trait ClusterDataPort: Send + Sync + 'static {
    fn overview(&self, metrics: bool) -> DataFuture<Overview, String>;

    fn metrics_probe(&self) -> DataFuture<(), MetricsError>;

    fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError>;

    fn metrics_pods(&self, namespace: Option<String>) -> DataFuture<Vec<PodMetric>, MetricsError>;

    fn metrics_node(self: Arc<Self>, name: String) -> DataFuture<Option<NodeMetric>, MetricsError> {
        Box::pin(async move {
            self.metrics_nodes()
                .await
                .map(|nodes| nodes.into_iter().find(|node| node.name == name))
        })
    }

    fn metrics_pod(
        self: Arc<Self>,
        namespace: String,
        name: String,
    ) -> DataFuture<Option<PodMetric>, MetricsError> {
        Box::pin(async move {
            self.metrics_pods(Some(namespace.clone()))
                .await
                .map(|pods| {
                    pods.into_iter()
                        .find(|pod| pod.namespace == namespace && pod.name == name)
                })
        })
    }

    fn namespaces(&self) -> DataFuture<Vec<String>, String>;

    fn cluster_uid(&self) -> DataFuture<String, String>;

    fn server_version(&self) -> DataFuture<String, String>;
}

#[derive(Clone)]
pub struct KubeClusterData {
    client: Client,
}

impl From<Client> for KubeClusterData {
    fn from(client: Client) -> Self {
        Self { client }
    }
}

async fn server_version_with_timeout(
    request: impl Future<Output = Result<String, String>>,
    timeout: Duration,
) -> Result<String, String> {
    with_read_timeout(
        timeout,
        request,
        "The API server version request timed out. Check the cluster connection and try again."
            .to_owned(),
    )
    .await
}

async fn collect_overview_results(
    mut tasks: JoinSet<OverviewTaskResult>,
    deadline: Duration,
) -> (
    HashMap<&'static str, ObjectListResult>,
    Option<Vec<NodeMetric>>,
) {
    let deadline_at = Instant::now() + deadline;
    let mut results: HashMap<&'static str, ObjectListResult> = HashMap::new();
    let mut usage = None;
    loop {
        match tokio::time::timeout_at(deadline_at, tasks.join_next()).await {
            Ok(Some(Ok(OverviewTaskResult::Objects(source, result)))) => {
                results.insert(source, result);
            }
            Ok(Some(Ok(OverviewTaskResult::Metrics(result)))) => {
                usage = result.ok();
            }
            Ok(Some(Err(_))) | Err(_) | Ok(None) => break,
        }
    }
    (results, usage)
}

async fn overview_with_deadline(
    client: Client,
    metrics: bool,
    deadline: Duration,
) -> Result<Overview, String> {
    let mut tasks = JoinSet::<OverviewTaskResult>::new();
    let resource_specs: [(&'static str, ApiResource); 7] = [
        ("pods", resource("", "v1", "Pod", "pods")),
        ("nodes", resource("", "v1", "Node", "nodes")),
        (
            "deployments",
            resource("apps", "v1", "Deployment", "deployments"),
        ),
        (
            "statefulsets",
            resource("apps", "v1", "StatefulSet", "statefulsets"),
        ),
        (
            "daemonsets",
            resource("apps", "v1", "DaemonSet", "daemonsets"),
        ),
        ("jobs", resource("batch", "v1", "Job", "jobs")),
        ("cronjobs", resource("batch", "v1", "CronJob", "cronjobs")),
    ];
    for (source, resource) in resource_specs {
        let client = client.clone();
        tasks.spawn(async move {
            OverviewTaskResult::Objects(source, list_objects(&client, &resource).await)
        });
    }
    if metrics {
        let client = client.clone();
        tasks.spawn(async move {
            OverviewTaskResult::Metrics(crate::metrics::fetch_nodes(&client).await)
        });
    }

    let (mut results, usage) = collect_overview_results(tasks, deadline).await;

    let timeout_reason = format!(
        "Overview request timed out after {}s. Check the cluster connection and try again.",
        deadline.as_secs()
    );
    for source in [
        "pods",
        "nodes",
        "deployments",
        "statefulsets",
        "daemonsets",
        "jobs",
        "cronjobs",
    ] {
        results
            .entry(source)
            .or_insert_with(|| Err(timeout_reason.clone()));
    }
    let mut take = |source: &'static str| {
        results
            .remove(source)
            .unwrap_or_else(|| Err(timeout_reason.clone()))
    };
    let pods = take("pods");
    let nodes = take("nodes");
    let deployments = take("deployments");
    let stateful_sets = take("statefulsets");
    let daemon_sets = take("daemonsets");
    let jobs = take("jobs");
    let cron_jobs = take("cronjobs");
    let input = OverviewInput {
        pods: object_set(&pods),
        nodes: object_set(&nodes),
        deployments: object_set(&deployments),
        stateful_sets: object_set(&stateful_sets),
        daemon_sets: object_set(&daemon_sets),
        jobs: object_set(&jobs),
        cron_jobs: object_set(&cron_jobs),
    };
    Ok(crate::overview::build(&input, usage.as_deref()))
}

impl ClusterDataPort for KubeClusterData {
    fn overview(&self, metrics: bool) -> DataFuture<Overview, String> {
        let client = self.client.clone();
        Box::pin(overview_with_deadline(client, metrics, OVERVIEW_DEADLINE))
    }

    fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
        let client = self.client.clone();
        Box::pin(async move { crate::metrics::probe(&client).await })
    }

    fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
        let client = self.client.clone();
        Box::pin(async move { crate::metrics::fetch_nodes(&client).await })
    }

    fn metrics_pods(&self, namespace: Option<String>) -> DataFuture<Vec<PodMetric>, MetricsError> {
        let client = self.client.clone();
        Box::pin(async move { crate::metrics::fetch_pods(&client, namespace.as_deref()).await })
    }

    fn metrics_node(self: Arc<Self>, name: String) -> DataFuture<Option<NodeMetric>, MetricsError> {
        let client = self.client.clone();
        Box::pin(async move { crate::metrics::fetch_node(&client, &name).await })
    }

    fn metrics_pod(
        self: Arc<Self>,
        namespace: String,
        name: String,
    ) -> DataFuture<Option<PodMetric>, MetricsError> {
        let client = self.client.clone();
        Box::pin(async move { crate::metrics::fetch_pod(&client, &namespace, &name).await })
    }

    fn namespaces(&self) -> DataFuture<Vec<String>, String> {
        let client = self.client.clone();
        Box::pin(async move {
            let api: Api<Namespace> = Api::all(client);
            let list = api
                .list(&ListParams::default())
                .await
                .map_err(|error| format!("Failed to list namespaces: {error}. Check the cluster connection and permissions, then try again."))?;
            let mut names = list
                .items
                .into_iter()
                .filter_map(|namespace| namespace.metadata.name)
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            Ok(names)
        })
    }

    fn cluster_uid(&self) -> DataFuture<String, String> {
        let client = self.client.clone();
        Box::pin(async move {
            let resource = ApiResource::from_gvk_with_plural(
                &GroupVersionKind::gvk("", "v1", "Namespace"),
                "namespaces",
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let namespace = api
                .get("kube-system")
                .await
                .map_err(|error| error.to_string())?;
            namespace
                .metadata
                .uid
                .ok_or_else(|| "The kube-system namespace had no UID. Check the cluster identity and try again.".to_owned())
        })
    }

    fn server_version(&self) -> DataFuture<String, String> {
        let client = self.client.clone();
        Box::pin(server_version_with_timeout(
            async move {
                client
                    .apiserver_version()
                    .await
                    .map(|info| info.git_version)
                    .map_err(|error| error.to_string())
            },
            NON_WATCH_READ_TIMEOUT,
        ))
    }
}

#[derive(Clone)]
pub struct ClusterDataSource(Arc<dyn ClusterDataPort>);

impl ClusterDataSource {
    pub fn from_port(port: Arc<dyn ClusterDataPort>) -> Self {
        Self(port)
    }

    pub fn port(&self) -> Arc<dyn ClusterDataPort> {
        Arc::clone(&self.0)
    }

    pub fn metrics_node(
        &self,
        name: impl Into<String>,
    ) -> DataFuture<Option<NodeMetric>, MetricsError> {
        self.0.clone().metrics_node(name.into())
    }

    pub fn metrics_pod(
        &self,
        namespace: impl Into<String>,
        name: impl Into<String>,
    ) -> DataFuture<Option<PodMetric>, MetricsError> {
        self.0.clone().metrics_pod(namespace.into(), name.into())
    }
}

impl From<Client> for ClusterDataSource {
    fn from(client: Client) -> Self {
        Self::from_port(Arc::new(KubeClusterData::from(client)))
    }
}

impl From<KubeClusterData> for ClusterDataSource {
    fn from(data: KubeClusterData) -> Self {
        Self::from_port(Arc::new(data))
    }
}

impl From<Arc<dyn ClusterDataPort>> for ClusterDataSource {
    fn from(port: Arc<dyn ClusterDataPort>) -> Self {
        Self::from_port(port)
    }
}

pub fn data_source_for_cluster(
    registry: &ClusterRegistry,
    cluster: ClusterId,
) -> Option<ClusterDataSource> {
    registry
        .get(cluster)
        .map(|cluster| ClusterDataSource::from(cluster.client().clone()))
}

fn object_set(result: &Result<Vec<Arc<DynamicObject>>, String>) -> ObjectSet<'_> {
    match result {
        Ok(objects) => ObjectSet::objects(objects),
        Err(reason) => ObjectSet::unavailable(reason),
    }
}

async fn list_objects(
    client: &Client,
    resource: &ApiResource,
) -> Result<Vec<Arc<DynamicObject>>, String> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), resource);
    let list = with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            api.list(&ListParams::default())
                .await
                .map_err(|error| format!("Failed to list {}: {error}. Check the cluster connection and permissions, then try again.", resource.plural))
        },
        format!(
            "Failed to list {}: request timed out after {}s. Check the cluster connection and try again.",
            resource.plural,
            NON_WATCH_READ_TIMEOUT.as_secs()
        ),
    )
    .await?;
    Ok(list.items.into_iter().map(Arc::new).collect())
}

fn resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
    let api_version = if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    };
    ApiResource {
        group: group.to_owned(),
        version: version.to_owned(),
        api_version,
        kind: kind.to_owned(),
        plural: plural.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePort;

    fn object(value: serde_json::Value) -> Arc<DynamicObject> {
        Arc::new(serde_json::from_value(value).expect("object"))
    }

    impl ClusterDataPort for FakePort {
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
            Box::pin(async { Ok(String::new()) })
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[test]
    fn one_kind_failure_keeps_other_overview_data() {
        let pods_error = "list pods: 401 Unauthorized".to_owned();
        let pods: Result<Vec<Arc<DynamicObject>>, String> = Err(pods_error.clone());
        let nodes = Ok(vec![object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": { "name": "node-a" },
            "status": { "conditions": [{ "type": "Ready", "status": "True" }] },
        }))]);
        let deployments: Result<Vec<Arc<DynamicObject>>, String> =
            Err("list deployments: 403 Forbidden".to_owned());
        let stateful_sets: Result<Vec<Arc<DynamicObject>>, String> = Ok(Vec::new());
        let daemon_sets: Result<Vec<Arc<DynamicObject>>, String> = Ok(Vec::new());
        let jobs: Result<Vec<Arc<DynamicObject>>, String> = Ok(Vec::new());
        let cron_jobs: Result<Vec<Arc<DynamicObject>>, String> = Ok(Vec::new());
        let overview = crate::overview::build(
            &OverviewInput {
                pods: object_set(&pods),
                nodes: object_set(&nodes),
                deployments: object_set(&deployments),
                stateful_sets: object_set(&stateful_sets),
                daemon_sets: object_set(&daemon_sets),
                jobs: object_set(&jobs),
                cron_jobs: object_set(&cron_jobs),
            },
            None,
        );

        assert_eq!(overview.nodes.count, 1);
        assert_eq!(overview.nodes.ready, 1);
        assert_eq!(overview.capacities.len(), 1);
        assert!(!overview.is_complete());
        assert_eq!(overview.level(), crate::overview::HealthLevel::Warning);
        assert_eq!(
            overview
                .unavailable_sources
                .iter()
                .map(|source| source.source)
                .collect::<Vec<_>>(),
            ["pods", "deployments"]
        );
        assert_eq!(overview.unavailable_sources[0].reason, pods_error);
    }

    #[tokio::test]
    async fn overview_deadline_keeps_completed_results() {
        let mut tasks = JoinSet::new();
        tasks
            .spawn(async { OverviewTaskResult::Objects("pods", Err("pods forbidden".to_owned())) });
        tasks.spawn(async {
            OverviewTaskResult::Objects(
                "nodes",
                Ok(vec![object(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Node",
                    "metadata": { "name": "node-a" },
                }))]),
            )
        });
        tasks.spawn(async { std::future::pending::<OverviewTaskResult>().await });

        let (results, usage) = tokio::time::timeout(
            Duration::from_secs(1),
            collect_overview_results(tasks, Duration::from_millis(50)),
        )
        .await
        .expect("the total deadline ends collection");

        assert!(usage.is_none());
        assert_eq!(results.get("pods"), Some(&Err("pods forbidden".to_owned())));
        let nodes = results.get("nodes").expect("nodes result");
        assert!(matches!(nodes, Ok(objects) if objects.len() == 1));
        assert!(!results.contains_key("jobs"));
    }

    #[tokio::test]
    async fn server_version_timeout_finishes() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            server_version_with_timeout(
                std::future::pending::<Result<String, String>>(),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("server version timeout must finish");
        assert_eq!(
            result,
            Err("The API server version request timed out. Check the cluster connection and try again.".to_owned())
        );
    }

    #[test]
    fn fake_port_can_be_injected() {
        let port: Arc<dyn ClusterDataPort> = Arc::new(FakePort);
        let source = ClusterDataSource::from_port(port.clone());
        assert!(Arc::ptr_eq(&port, &source.port()));
    }

    #[tokio::test]
    async fn source_exposes_typed_namespace_names() {
        let source = ClusterDataSource::from_port(Arc::new(FakePort));
        assert_eq!(
            source.port().namespaces().await.expect("namespace names"),
            vec!["default".to_owned(), "kube-system".to_owned()]
        );
    }
}
