//! Builds, groups, and searches the cluster resource catalog.

use std::cmp::Reverse;
use std::time::Duration;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    SubjectAccessReviewStatus,
};
use kube::Client;
use kube::api::{Api, PostParams};
use kube::core::Version;
use kube::core::gvk::GroupVersionKind;
use kube::discovery::{ApiCapabilities, ApiResource, Discovery, Scope};
use serde::{Deserialize, Serialize};

use crate::cluster::{Cluster, ClusterError};

/// Display name for the core group.
pub const CORE_GROUP: &str = "core";

pub const DISCOVERY_DEADLINE: Duration = Duration::from_secs(30);
const AGGREGATED_DISCOVERY_DEADLINE: Duration = Duration::from_secs(8);

pub type ResourceIdentity = GroupVersionKind;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("Failed to read cluster discovery: {0}. Check the cluster connection and try again.")]
    Discovery(#[from] ClusterError),

    #[error(
        "Cluster discovery timed out after {} seconds. Check the cluster connection and try again.",
        (.0).as_secs()
    )]
    Timeout(Duration),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RbacCapabilities {
    pub list: bool,
    pub watch: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RbacError {
    #[error("RBAC access review failed: {0}. Check the request and try again.")]
    Api(#[source] kube::Error),

    #[error("RBAC access review returned no status. Check the API server response and try again.")]
    MissingStatus,

    #[error("RBAC access review was not completed: {0}. Check the request and try again.")]
    Evaluation(String),

    #[error(
        "A cluster-scoped resource cannot specify a namespace. Remove the namespace and try again."
    )]
    InvalidNamespace,
}

/// Resource scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceScope {
    Namespaced,
    Cluster,
}

impl ResourceScope {
    pub fn is_namespaced(self) -> bool {
        matches!(self, Self::Namespaced)
    }
}

impl From<Scope> for ResourceScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Namespaced => Self::Namespaced,
            Scope::Cluster => Self::Cluster,
        }
    }
}

/// One `(group, version, kind)` entry from discovery.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceEntry {
    /// Original kube group name. The core group is an empty string.
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub scope: ResourceScope,
    pub verbs: Vec<String>,
    /// Short names, such as `po` for `pods`. The current discovery source leaves them empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub short_names: Vec<String>,
}

impl ResourceEntry {
    fn from_discovery(resource: &ApiResource, capabilities: &ApiCapabilities) -> Self {
        Self {
            group: resource.group.clone(),
            version: resource.version.clone(),
            kind: resource.kind.clone(),
            plural: resource.plural.clone(),
            scope: capabilities.scope.clone().into(),
            verbs: capabilities.operations.clone(),
            short_names: Vec::new(),
        }
    }

    /// API version, such as `v1` or `apps/v1`.
    pub fn api_version(&self) -> String {
        api_version(&self.group, &self.version)
    }

    pub fn namespaced(&self) -> bool {
        self.scope.is_namespaced()
    }

    pub fn supports(&self, verb: &str) -> bool {
        self.verbs.iter().any(|supported| supported == verb)
    }

    /// Display name for the group. The core group is `core`.
    pub fn group_display(&self) -> &str {
        if self.group.is_empty() {
            CORE_GROUP
        } else {
            &self.group
        }
    }

    /// Exact GVK. The core group uses an empty string.
    pub fn gvk(&self) -> GroupVersionKind {
        GroupVersionKind::gvk(&self.group, &self.version, &self.kind)
    }

    pub fn identity(&self) -> ResourceIdentity {
        self.gvk()
    }

    /// Build the [`ApiResource`] needed by a dynamic `Api`.
    pub fn to_api_resource(&self) -> ApiResource {
        ApiResource {
            group: self.group.clone(),
            version: self.version.clone(),
            api_version: self.api_version(),
            kind: self.kind.clone(),
            plural: self.plural.clone(),
        }
    }
}

pub async fn rbac_capabilities(
    cluster: &Cluster,
    entry: &ResourceEntry,
    namespace: Option<&str>,
) -> Result<RbacCapabilities, RbacError> {
    if !entry.namespaced() && namespace.is_some() {
        return Err(RbacError::InvalidNamespace);
    }
    let (list, watch) = tokio::try_join!(
        review_access(
            cluster.client(),
            entry,
            namespace,
            "list",
            entry.supports("list"),
        ),
        review_access(
            cluster.client(),
            entry,
            namespace,
            "watch",
            entry.supports("watch"),
        ),
    )?;
    Ok(RbacCapabilities { list, watch })
}

async fn review_access(
    client: &Client,
    entry: &ResourceEntry,
    namespace: Option<&str>,
    verb: &str,
    advertised: bool,
) -> Result<bool, RbacError> {
    if !advertised {
        return Ok(false);
    }
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let review = api
        .create(
            &PostParams::default(),
            &SelfSubjectAccessReview {
                metadata: Default::default(),
                spec: SelfSubjectAccessReviewSpec {
                    resource_attributes: Some(access_attributes(entry, namespace, verb)),
                    non_resource_attributes: None,
                },
                status: None,
            },
        )
        .await
        .map_err(RbacError::Api)?;
    let status = review.status.ok_or(RbacError::MissingStatus)?;
    access_allowed(status)
}

fn access_attributes(
    entry: &ResourceEntry,
    namespace: Option<&str>,
    verb: &str,
) -> ResourceAttributes {
    ResourceAttributes {
        group: Some(entry.group.clone()),
        version: Some(entry.version.clone()),
        resource: Some(entry.plural.clone()),
        namespace: namespace.map(str::to_owned),
        verb: Some(verb.to_owned()),
        ..Default::default()
    }
}

fn access_allowed(status: SubjectAccessReviewStatus) -> Result<bool, RbacError> {
    if status.allowed {
        return Ok(true);
    }
    if status.denied == Some(true) {
        return Ok(false);
    }
    if let Some(error) = status.evaluation_error {
        return Err(RbacError::Evaluation(error));
    }
    Ok(false)
}

/// Resources for one API version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceVersion {
    pub version: String,
    pub resources: Vec<ResourceEntry>,
}

/// Grouped view of one API group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceGroup {
    /// Original kube group name. The core group is an empty string.
    pub group: String,
    /// Preferred API version declared by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_version: Option<String>,
    /// Preferred version first, then kube priority order.
    pub versions: Vec<ResourceVersion>,
}

impl ResourceGroup {
    /// Display name for the core group.
    pub fn display_name(&self) -> &str {
        if self.group.is_empty() {
            CORE_GROUP
        } else {
            &self.group
        }
    }
}

/// Catalog filter. Events are excluded by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogFilter {
    /// Excluded `(group, plural)` pairs. The core group is an empty string.
    excluded: Vec<(String, String)>,
}

impl Default for CatalogFilter {
    fn default() -> Self {
        Self::browsable()
    }
}

impl CatalogFilter {
    /// Browsing excludes events from the core and `events.k8s.io` groups.
    pub fn browsable() -> Self {
        Self::unfiltered()
            .exclude("", "events")
            .exclude("events.k8s.io", "events")
    }

    /// Do not exclude resources. Subresources are still removed.
    pub fn unfiltered() -> Self {
        Self {
            excluded: Vec::new(),
        }
    }

    /// Add one exclusion. Use an empty string for the core group.
    pub fn exclude(mut self, group: &str, plural: &str) -> Self {
        self.excluded.push((group.to_string(), plural.to_string()));
        self
    }

    fn excludes(&self, group: &str, plural: &str) -> bool {
        self.excluded
            .iter()
            .any(|(excluded_group, excluded_plural)| {
                excluded_group == group && excluded_plural == plural
            })
    }
}

/// Full resource catalog grouped by group, version, and resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCatalog {
    /// Groups are sorted by name. Empty groups and versions are removed.
    groups: Vec<ResourceGroup>,
}

impl ResourceCatalog {
    /// Build from the cluster discovery cache. This can start a full discovery request.
    pub async fn fetch(cluster: &Cluster) -> Result<Self, CatalogError> {
        Self::load(cluster, false, &CatalogFilter::default()).await
    }

    /// Run discovery again and build a fresh catalog.
    pub async fn fetch_fresh(cluster: &Cluster) -> Result<Self, CatalogError> {
        Self::load(cluster, true, &CatalogFilter::default()).await
    }

    /// Build a catalog with a custom filter.
    pub async fn fetch_with_filter(
        cluster: &Cluster,
        filter: &CatalogFilter,
    ) -> Result<Self, CatalogError> {
        Self::load(cluster, false, filter).await
    }

    async fn load(
        cluster: &Cluster,
        fresh: bool,
        filter: &CatalogFilter,
    ) -> Result<Self, CatalogError> {
        let result = tokio::time::timeout(DISCOVERY_DEADLINE, async {
            if fresh {
                let discovery =
                    discover(cluster.client())
                        .await
                        .map_err(|source| ClusterError::Discovery {
                            cluster: cluster.name().to_string(),
                            source: Box::new(source),
                        })?;
                Ok(Self::from_discovery(&discovery, filter))
            } else {
                Ok(Self::from_discovery(cluster.discovery().await?, filter))
            }
        })
        .await;

        match result {
            Ok(result) => result,
            Err(_) => Err(CatalogError::Timeout(DISCOVERY_DEADLINE)),
        }
    }

    fn from_discovery(discovery: &Discovery, filter: &CatalogFilter) -> Self {
        let groups = discovery
            .groups_alphabetical()
            .into_iter()
            .map(|group| ResourceGroup {
                group: group.name().to_string(),
                preferred_version: group.preferred_version().map(str::to_string),
                versions: group
                    .versions()
                    .map(|version| ResourceVersion {
                        version: version.to_string(),
                        resources: group
                            .versioned_resources(version)
                            .iter()
                            .map(|(resource, capabilities)| {
                                ResourceEntry::from_discovery(resource, capabilities)
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        Self::build(groups, filter)
    }

    /// Filter and sort groups, versions, and resources.
    fn build(mut groups: Vec<ResourceGroup>, filter: &CatalogFilter) -> Self {
        groups.sort_by(|left, right| left.group.cmp(&right.group));
        for group in &mut groups {
            let preferred = group.preferred_version.clone();
            group.versions.sort_by_cached_key(|version| {
                (
                    preferred.as_deref() != Some(version.version.as_str()),
                    Reverse(Version::parse(version.version.as_str()).priority()),
                    version.version.clone(),
                )
            });
            group.versions.retain_mut(|version| {
                version.resources.retain(|entry| {
                    !entry.plural.contains('/') && !filter.excludes(&entry.group, &entry.plural)
                });
                version.resources.sort_by(|left, right| {
                    left.plural
                        .cmp(&right.plural)
                        .then_with(|| left.kind.cmp(&right.kind))
                        .then_with(|| left.group.cmp(&right.group))
                        .then_with(|| left.version.cmp(&right.version))
                });
                !version.resources.is_empty()
            });
        }
        groups.retain(|group| !group.versions.is_empty());
        Self { groups }
    }

    pub fn groups(&self) -> &[ResourceGroup] {
        &self.groups
    }

    /// Flat view of all entries in group order.
    pub fn entries(&self) -> impl Iterator<Item = &ResourceEntry> {
        self.groups
            .iter()
            .flat_map(|group| group.versions.iter())
            .flat_map(|version| version.resources.iter())
    }

    pub fn searchable_entries(&self) -> Vec<ResourceEntry> {
        self.groups
            .iter()
            .filter_map(|group| {
                let preferred = group.preferred_version.as_deref()?;
                group
                    .versions
                    .iter()
                    .find(|version| version.version == preferred)
            })
            .flat_map(|version| version.resources.iter())
            .filter(|entry| !entry.plural.contains('/') && entry.supports("list"))
            .cloned()
            .collect()
    }

    /// Return the first matching kind. Use [`ResourceCatalog::resolve`] for an exact version.
    pub fn by_kind(&self, kind: &str) -> Option<&ResourceEntry> {
        self.entries().find(|entry| entry.kind == kind)
    }

    pub fn by_plural(&self, plural: &str) -> Option<&ResourceEntry> {
        self.entries().find(|entry| entry.plural == plural)
    }

    pub fn by_gvk(&self, gvk: &GroupVersionKind) -> Option<&ResourceEntry> {
        self.resolve(gvk)
    }

    pub fn resolve(&self, gvk: &GroupVersionKind) -> Option<&ResourceEntry> {
        self.entries().find(|entry| entry.gvk() == *gvk)
    }
}

fn api_version(group: &str, version: &str) -> String {
    if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    }
}

async fn discover(client: &Client) -> Result<Discovery, kube::Error> {
    let aggregated = tokio::time::timeout(
        AGGREGATED_DISCOVERY_DEADLINE,
        Discovery::new(client.clone()).run_aggregated(),
    )
    .await;

    match aggregated {
        Ok(Ok(discovery)) => Ok(discovery),
        Ok(Err(_)) | Err(_) => Discovery::new(client.clone()).run().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        scope: ResourceScope,
        verbs: &[&str],
    ) -> ResourceEntry {
        ResourceEntry {
            group: group.to_string(),
            version: version.to_string(),
            kind: kind.to_string(),
            plural: plural.to_string(),
            scope,
            verbs: verbs.iter().map(|verb| verb.to_string()).collect(),
            short_names: Vec::new(),
        }
    }

    fn version(version: &str, resources: Vec<ResourceEntry>) -> ResourceVersion {
        ResourceVersion {
            version: version.to_string(),
            resources,
        }
    }

    fn group(
        group: &str,
        preferred_version: Option<&str>,
        versions: Vec<ResourceVersion>,
    ) -> ResourceGroup {
        ResourceGroup {
            group: group.to_string(),
            preferred_version: preferred_version.map(str::to_string),
            versions,
        }
    }

    fn sample_groups() -> Vec<ResourceGroup> {
        vec![
            group(
                "apps",
                Some("v1"),
                vec![version(
                    "v1",
                    vec![
                        entry(
                            "apps",
                            "v1",
                            "Deployment",
                            "deployments",
                            ResourceScope::Namespaced,
                            &["get", "list", "watch"],
                        ),
                        entry(
                            "apps",
                            "v1",
                            "ReplicaSet",
                            "replicasets",
                            ResourceScope::Namespaced,
                            &["get", "list"],
                        ),
                    ],
                )],
            ),
            group(
                "",
                Some("v1"),
                vec![version(
                    "v1",
                    vec![
                        entry(
                            "",
                            "v1",
                            "Pod",
                            "pods",
                            ResourceScope::Namespaced,
                            &["get", "list", "watch"],
                        ),
                        entry(
                            "",
                            "v1",
                            "Node",
                            "nodes",
                            ResourceScope::Cluster,
                            &["get", "list"],
                        ),
                        entry(
                            "",
                            "v1",
                            "Event",
                            "events",
                            ResourceScope::Namespaced,
                            &["get", "list"],
                        ),
                        entry(
                            "",
                            "v1",
                            "Pod",
                            "pods/status",
                            ResourceScope::Namespaced,
                            &["get"],
                        ),
                    ],
                )],
            ),
            group(
                "events.k8s.io",
                None,
                vec![version(
                    "v1",
                    vec![entry(
                        "events.k8s.io",
                        "v1",
                        "Event",
                        "events",
                        ResourceScope::Namespaced,
                        &["get", "list"],
                    )],
                )],
            ),
            group(
                "example.com",
                Some("v1"),
                vec![
                    version(
                        "v1beta1",
                        vec![entry(
                            "example.com",
                            "v1beta1",
                            "Widget",
                            "widgets",
                            ResourceScope::Namespaced,
                            &["get", "list"],
                        )],
                    ),
                    version(
                        "v1",
                        vec![
                            entry(
                                "example.com",
                                "v1",
                                "Widget",
                                "widgets",
                                ResourceScope::Namespaced,
                                &["get", "list", "create"],
                            ),
                            entry(
                                "example.com",
                                "v1",
                                "Gadget",
                                "gadgets",
                                ResourceScope::Cluster,
                                &["get", "list"],
                            ),
                        ],
                    ),
                ],
            ),
        ]
    }

    fn sample_catalog() -> ResourceCatalog {
        ResourceCatalog::build(sample_groups(), &CatalogFilter::default())
    }

    #[test]
    fn timeout_error_uses_seconds() {
        assert_eq!(
            CatalogError::Timeout(Duration::from_secs(30)).to_string(),
            "Cluster discovery timed out after 30 seconds. Check the cluster connection and try again."
        );
    }

    #[test]
    fn groups_follow_kubectl_order_with_core_first() {
        let catalog = sample_catalog();
        let names: Vec<&str> = catalog
            .groups()
            .iter()
            .map(ResourceGroup::display_name)
            .collect();
        assert_eq!(names, ["core", "apps", "example.com"]);
    }

    #[test]
    fn default_filter_drops_events_and_subresources() {
        let catalog = sample_catalog();
        assert!(catalog.by_plural("events").is_none());
        assert!(catalog.by_plural("pods/status").is_none());
        assert!(catalog.by_plural("pods").is_some());
        assert!(
            catalog
                .groups()
                .iter()
                .all(|group| group.group != "events.k8s.io"),
            "remove groups that contain only events"
        );
        assert_eq!(
            catalog.entries().count(),
            7,
            "two widget versions count as two entries"
        );
    }

    #[test]
    fn unfiltered_keeps_events_but_still_drops_subresources() {
        let catalog = ResourceCatalog::build(sample_groups(), &CatalogFilter::unfiltered());
        assert!(catalog.by_plural("events").is_some());
        assert_eq!(
            catalog
                .groups()
                .iter()
                .filter(|group| group.group == "events.k8s.io")
                .count(),
            1
        );
        assert!(catalog.by_plural("pods/status").is_none());
    }

    #[test]
    fn custom_filter_excludes_extra_resources() {
        let filter = CatalogFilter::default().exclude("apps", "deployments");
        let catalog = ResourceCatalog::build(sample_groups(), &filter);
        assert!(catalog.by_plural("deployments").is_none());
        assert!(catalog.by_plural("replicasets").is_some());
    }

    #[test]
    fn preferred_version_is_listed_first() {
        let catalog = sample_catalog();
        let example = catalog
            .groups()
            .iter()
            .find(|group| group.group == "example.com")
            .expect("example.com group");
        let versions: Vec<&str> = example
            .versions
            .iter()
            .map(|version| version.version.as_str())
            .collect();
        assert_eq!(versions, ["v1", "v1beta1"]);
        assert_eq!(example.preferred_version.as_deref(), Some("v1"));
    }

    #[test]
    fn searchable_entries_use_preferred_versions_and_listable_resources() {
        let entries = sample_catalog().searchable_entries();
        let identities: Vec<_> = entries.iter().map(ResourceEntry::identity).collect();
        assert!(identities.contains(&GroupVersionKind::gvk("", "v1", "Pod")));
        assert!(identities.contains(&GroupVersionKind::gvk("apps", "v1", "Deployment")));
        assert!(identities.contains(&GroupVersionKind::gvk("example.com", "v1", "Widget")));
        assert!(!identities.contains(&GroupVersionKind::gvk("example.com", "v1beta1", "Widget")));
        assert!(entries.iter().all(|entry| entry.supports("list")));
    }

    #[test]
    fn searchable_entries_skip_groups_without_a_preferred_version() {
        let catalog = ResourceCatalog::build(
            vec![group(
                "unversioned.example",
                None,
                vec![version(
                    "v1",
                    vec![entry(
                        "unversioned.example",
                        "v1",
                        "Widget",
                        "widgets",
                        ResourceScope::Namespaced,
                        &["list"],
                    )],
                )],
            )],
            &CatalogFilter::unfiltered(),
        );
        assert!(catalog.searchable_entries().is_empty());
    }

    #[test]
    fn same_kind_in_different_groups_keeps_distinct_gvks() {
        let catalog = ResourceCatalog::build(
            vec![
                group(
                    "z.example",
                    Some("v1"),
                    vec![version(
                        "v1",
                        vec![entry(
                            "z.example",
                            "v1",
                            "Widget",
                            "widgets",
                            ResourceScope::Namespaced,
                            &["list"],
                        )],
                    )],
                ),
                group(
                    "a.example",
                    Some("v1"),
                    vec![version(
                        "v1",
                        vec![entry(
                            "a.example",
                            "v1",
                            "Widget",
                            "widgets",
                            ResourceScope::Cluster,
                            &["list"],
                        )],
                    )],
                ),
            ],
            &CatalogFilter::unfiltered(),
        );
        let widgets: Vec<ResourceIdentity> = catalog
            .entries()
            .filter(|entry| entry.kind == "Widget")
            .map(ResourceEntry::identity)
            .collect();
        assert_eq!(
            widgets,
            vec![
                GroupVersionKind::gvk("a.example", "v1", "Widget"),
                GroupVersionKind::gvk("z.example", "v1", "Widget"),
            ]
        );
        assert_eq!(
            catalog
                .by_gvk(&GroupVersionKind::gvk("z.example", "v1", "Widget"))
                .map(ResourceEntry::group_display),
            Some("z.example")
        );
        assert_eq!(
            catalog
                .by_gvk(&GroupVersionKind::gvk("a.example", "v1", "Widget"))
                .map(|entry| entry.scope),
            Some(ResourceScope::Cluster)
        );
    }

    #[test]
    fn preferred_version_is_selected_for_a_kind_without_losing_other_versions() {
        let catalog = sample_catalog();
        let widget = catalog
            .resolve(&GroupVersionKind::gvk("example.com", "v1", "Widget"))
            .expect("example.com Widget");
        assert_eq!(widget.version, "v1");
        assert_eq!(
            widget.identity(),
            GroupVersionKind::gvk("example.com", "v1", "Widget")
        );
        assert!(
            catalog
                .resolve(&GroupVersionKind::gvk("example.com", "v1beta1", "Widget"))
                .is_some()
        );
    }

    #[test]
    fn namespace_scope_is_kept_per_gvk() {
        let catalog = ResourceCatalog::build(
            vec![
                group(
                    "a.example",
                    Some("v1"),
                    vec![version(
                        "v1",
                        vec![entry(
                            "a.example",
                            "v1",
                            "Thing",
                            "things",
                            ResourceScope::Namespaced,
                            &["list"],
                        )],
                    )],
                ),
                group(
                    "b.example",
                    Some("v1"),
                    vec![version(
                        "v1",
                        vec![entry(
                            "b.example",
                            "v1",
                            "Thing",
                            "things",
                            ResourceScope::Cluster,
                            &["list"],
                        )],
                    )],
                ),
            ],
            &CatalogFilter::unfiltered(),
        );
        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("a.example", "v1", "Thing"))
                .map(ResourceEntry::namespaced),
            Some(true)
        );
        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("b.example", "v1", "Thing"))
                .map(ResourceEntry::namespaced),
            Some(false)
        );
    }

    #[test]
    fn catalog_sorting_is_independent_of_input_order() {
        let forward = sample_catalog();
        let mut reverse_groups = sample_groups();
        reverse_groups.reverse();
        for group in &mut reverse_groups {
            group.versions.reverse();
            for version in &mut group.versions {
                version.resources.reverse();
            }
        }
        let reverse = ResourceCatalog::build(reverse_groups, &CatalogFilter::default());
        assert_eq!(forward, reverse);
        let groups: Vec<&str> = forward
            .groups()
            .iter()
            .map(|group| group.group.as_str())
            .collect();
        assert_eq!(groups, ["", "apps", "example.com"]);
    }

    #[test]
    fn lookups_by_kind_plural_gvk_and_scope() {
        let catalog = sample_catalog();

        let pods = catalog.by_kind("Pod").expect("Pod");
        assert_eq!(pods.plural, "pods");
        assert_eq!(pods.api_version(), "v1");

        let deployments = catalog.by_plural("deployments").expect("deployments");
        assert_eq!(
            (deployments.group.as_str(), deployments.version.as_str()),
            ("apps", "v1")
        );
        assert_eq!(deployments.api_version(), "apps/v1");

        assert!(
            catalog
                .by_plural("gadgets")
                .expect("gadgets")
                .verbs
                .iter()
                .any(|verb| verb == "list")
        );
        assert!(catalog.by_kind("Missing").is_none());

        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("apps", "v1", "Deployment"))
                .map(|entry| entry.kind.as_str()),
            Some("Deployment")
        );
        assert!(
            catalog
                .resolve(&GroupVersionKind::gvk("apps", "v1beta1", "Deployment"))
                .is_none()
        );

        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("", "v1", "Pod"))
                .map(ResourceEntry::namespaced),
            Some(true)
        );
        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("", "v1", "Node"))
                .map(ResourceEntry::namespaced),
            Some(false)
        );
        assert!(
            catalog
                .resolve(&GroupVersionKind::gvk("", "v1", "Missing"))
                .is_none()
        );
    }

    #[test]
    fn entry_exposes_api_resource_for_controller() {
        let catalog = sample_catalog();
        let nodes = catalog.by_kind("Node").expect("Node");
        assert_eq!(nodes.group_display(), "core");
        assert!(!nodes.namespaced());
        assert!(nodes.supports("list"));
        assert!(!nodes.supports("delete"));

        let resource = nodes.to_api_resource();
        assert_eq!(resource.group, "");
        assert_eq!(resource.api_version, "v1");
        assert_eq!(resource.plural, "nodes");
        assert_eq!(resource.kind, "Node");
    }

    #[test]
    fn rbac_attributes_use_plural_group_version_and_scope() {
        let deployment = entry(
            "apps",
            "v1",
            "Deployment",
            "deployments",
            ResourceScope::Namespaced,
            &["list", "watch"],
        );
        let attributes = access_attributes(&deployment, Some("prod"), "watch");
        assert_eq!(attributes.group.as_deref(), Some("apps"));
        assert_eq!(attributes.version.as_deref(), Some("v1"));
        assert_eq!(attributes.resource.as_deref(), Some("deployments"));
        assert_eq!(attributes.namespace.as_deref(), Some("prod"));
        assert_eq!(attributes.verb.as_deref(), Some("watch"));

        let pod = entry(
            "",
            "v1",
            "Pod",
            "pods",
            ResourceScope::Namespaced,
            &["list"],
        );
        let attributes = access_attributes(&pod, None, "list");
        assert_eq!(attributes.group.as_deref(), Some(""));
        assert_eq!(attributes.namespace, None);
    }

    #[test]
    fn rbac_status_preserves_allowed_denied_and_evaluation_errors() {
        assert!(
            access_allowed(SubjectAccessReviewStatus {
                allowed: true,
                denied: Some(true),
                evaluation_error: Some("partial".to_owned()),
                ..Default::default()
            })
            .expect("allowed")
        );
        assert!(
            !access_allowed(SubjectAccessReviewStatus {
                allowed: false,
                denied: Some(true),
                ..Default::default()
            })
            .expect("denied")
        );
        assert!(
            !access_allowed(SubjectAccessReviewStatus {
                allowed: false,
                denied: Some(false),
                ..Default::default()
            })
            .expect("no opinion")
        );
        assert!(matches!(
            access_allowed(SubjectAccessReviewStatus {
                allowed: false,
                evaluation_error: Some("authorizer unavailable".to_owned()),
                ..Default::default()
            }),
            Err(RbacError::Evaluation(_))
        ));
    }

    #[test]
    fn catalog_round_trips_through_json() {
        let catalog = sample_catalog();
        let json = serde_json::to_string(&catalog).expect("serialize");
        let restored: ResourceCatalog = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, catalog);
        assert_eq!(restored.entries().count(), catalog.entries().count());
    }

    #[test]
    fn empty_catalog_is_empty() {
        let catalog = ResourceCatalog::build(Vec::new(), &CatalogFilter::default());
        assert!(catalog.entries().next().is_none());
        assert!(catalog.by_kind("Pod").is_none());
    }

    fn kubeconfig_present() -> bool {
        crate::cluster::kubeconfig_present()
    }

    #[tokio::test]
    #[ignore = "Requires a kind cluster: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn kind_cluster_catalog_covers_core_and_apps() {
        if !kubeconfig_present() {
            return;
        }
        let registry = crate::cluster::ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry.clusters().first() else {
            return;
        };

        let catalog = ResourceCatalog::fetch(cluster)
            .await
            .expect("discovery succeeds");

        let pods = catalog.by_plural("pods").expect("core-group Pods");
        assert_eq!(pods.group, "");
        assert_eq!(pods.api_version(), "v1");
        assert_eq!(pods.scope, ResourceScope::Namespaced);
        assert!(pods.supports("list") && pods.supports("watch"));

        let deployments = catalog.by_plural("deployments").expect("apps deployments");
        assert_eq!(
            (deployments.group.as_str(), deployments.version.as_str()),
            ("apps", "v1")
        );
        assert!(deployments.namespaced());

        let services = catalog.by_plural("services").expect("core-group services");
        assert_eq!(services.api_version(), "v1");

        let nodes = catalog.by_kind("Node").expect("cluster-scoped Node");
        assert_eq!(nodes.scope, ResourceScope::Cluster);

        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("", "v1", "Pod"))
                .map(ResourceEntry::namespaced),
            Some(true)
        );
        assert_eq!(
            catalog
                .resolve(&GroupVersionKind::gvk("", "v1", "Node"))
                .map(ResourceEntry::namespaced),
            Some(false)
        );

        assert!(
            catalog.by_plural("events").is_none(),
            "the default filter excludes events"
        );

        let core = catalog
            .groups()
            .iter()
            .find(|group| group.group.is_empty())
            .expect("core group");
        assert_eq!(core.display_name(), "core");
        assert!(core.versions.iter().any(|version| version.version == "v1"));
    }

    const TEST_CRD_GROUP: &str = "k8sgpui.dev";
    const TEST_CRD_PLURAL: &str = "k8sgpuitests";
    const TEST_CRD_NAME: &str = "k8sgpuitests.k8sgpui.dev";
    const TEST_CRD_API_VERSION: &str = "k8sgpui.dev/v1";
    const TEST_CRD_KIND: &str = "K8sGpuiTest";

    fn test_crd()
    -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition
    {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": { "name": TEST_CRD_NAME },
            "spec": {
                "group": TEST_CRD_GROUP,
                "scope": "Namespaced",
                "names": {
                    "plural": TEST_CRD_PLURAL,
                    "singular": "k8sgpuitest",
                    "kind": TEST_CRD_KIND,
                    "shortNames": ["kgt"],
                },
                "versions": [{
                    "name": "v1",
                    "served": true,
                    "storage": true,
                    "schema": {
                        "openAPIV3Schema": {
                            "type": "object",
                            "x-kubernetes-preserve-unknown-fields": true,
                        },
                    },
                }],
            },
        }))
        .expect("valid CRD")
    }

    /// Poll until the API server serves the CRD group.
    async fn wait_until_crd_served(client: &kube::Client, api_version: &str, plural: &str) {
        loop {
            if let Ok(list) = client.list_api_group_resources(api_version).await
                && list
                    .resources
                    .iter()
                    .any(|resource| resource.name == plural)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    #[tokio::test]
    #[ignore = "Requires kind. Creates and deletes test CRD k8sgpuitests.k8sgpui.dev"]
    async fn installed_crd_appears_in_fresh_catalog() {
        use kube::api::{Api, DeleteParams, PostParams};

        if !kubeconfig_present() {
            return;
        }
        let registry = crate::cluster::ClusterRegistry::load_default()
            .await
            .expect("kubeconfig is readable");
        let Some(cluster) = registry.clusters().first() else {
            return;
        };

        let crds: Api<_> = Api::all(cluster.client().clone());
        let _ = crds.delete(TEST_CRD_NAME, &DeleteParams::default()).await;

        // Warm the cluster discovery cache before fetching fresh data.
        let cached = ResourceCatalog::fetch(cluster)
            .await
            .expect("discovery succeeds");
        assert!(
            cached.by_kind(TEST_CRD_KIND).is_none(),
            "no test CRD remains before the test"
        );

        crds.create(&PostParams::default(), &test_crd())
            .await
            .expect("create the test CRD");

        let fetched = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            wait_until_crd_served(cluster.client(), TEST_CRD_API_VERSION, TEST_CRD_PLURAL).await;
            ResourceCatalog::fetch_fresh(cluster).await
        })
        .await;

        let deleted = crds.delete(TEST_CRD_NAME, &DeleteParams::default()).await;

        let catalog = fetched
            .expect("the test CRD is discoverable within 60 seconds")
            .expect("fetch_fresh succeeds");
        assert!(deleted.is_ok(), "the test deletes the CRD: {deleted:?}");

        let entry = catalog
            .by_kind(TEST_CRD_KIND)
            .expect("the CRD appears in the catalog");
        assert_eq!(entry.group, TEST_CRD_GROUP);
        assert_eq!(entry.version, "v1");
        assert_eq!(entry.plural, TEST_CRD_PLURAL);
        assert_eq!(entry.scope, ResourceScope::Namespaced);
        assert_eq!(entry.api_version(), TEST_CRD_API_VERSION);
        assert!(entry.supports("list") && entry.supports("create"));

        let group = catalog
            .groups()
            .iter()
            .find(|group| group.group == TEST_CRD_GROUP)
            .expect("the CRD group appears in the grouped view");
        assert_eq!(group.display_name(), TEST_CRD_GROUP);
        assert_eq!(
            group
                .versions
                .first()
                .map(|version| version.version.as_str()),
            Some("v1")
        );
    }
}
