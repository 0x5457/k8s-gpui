//! Builds, groups, and searches the cluster resource catalog.

use std::cmp::Reverse;
use std::time::Duration;

use kube::Client;
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

    pub fn by_gvk(&self, gvk: &GroupVersionKind) -> Option<&ResourceEntry> {
        self.resolve(gvk)
    }

    pub fn resolve(&self, gvk: &GroupVersionKind) -> Option<&ResourceEntry> {
        self.entries().find(|entry| entry.gvk() == *gvk)
    }
}

/// `group/version`, or the bare version for the core group.
pub(crate) fn api_version(group: &str, version: &str) -> String {
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
    fn lookups_by_gvk_and_scope() {
        let catalog = sample_catalog();

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
}
