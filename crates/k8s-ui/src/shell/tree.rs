//! Resource tree and flattened visible rows.
//!
//! The tree uses a discovery catalog or demo data. The caller owns the set of
//! collapsed row IDs, which keeps catalog data separate from UI state.
//!
//! The core API group keeps its own row so its Kinds stay visible. A cluster with
//! many API groups lists them behind one collapsed container row instead, so the
//! sidebar does not turn into a wall of group names.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use gpui::SharedString;
use k8s_core::discovery::{CORE_GROUP, ResourceCatalog, ResourceEntry, ResourceIdentity};
use k8s_core::fuzzy;
use kube_core::Version;

use crate::design;

/// Label of the single container row that holds every non-core API group.
const API_GROUPS_LABEL: &str = "All API groups";

/// API groups up to this count keep their own rows. A longer list hides behind
/// the container so a cluster with many CRD groups cannot bury the Kinds that
/// stay visible above it.
const API_GROUPS_INLINE_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRowKind {
    Cluster,
    Group,
    Kind,
    /// Always-visible Overview row for the active cluster.
    Overview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub id: SharedString,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub depth: u8,
    pub kind: TreeRowKind,
    pub expanded: bool,
    /// One-based position among the visible rows that share this row's parent.
    pub pos_in_set: usize,
    /// Number of visible rows that share this row's parent.
    pub set_size: usize,
    /// Singular API kind for Kind rows. Group and Cluster rows use None.
    pub resource_kind: Option<SharedString>,
    pub resource_gvk: Option<ResourceIdentity>,
    /// Catalog entry for a Kind row. Demo rows have no entry.
    pub entry: Option<ResourceEntry>,
}

impl TreeRow {
    pub fn expandable(&self) -> bool {
        matches!(self.kind, TreeRowKind::Cluster | TreeRowKind::Group)
    }
}

/// Number the visible rows so every row reports its ARIA set position and size.
///
/// A flattened depth-first row list keeps a parent's children contiguous, so one
/// stack pass finds each parent and a second pass counts the children per parent.
/// Runs of equal depth are not enough: a container's own children sit between the
/// container and its siblings. Renumbering after filtering keeps the reported set
/// honest instead of leaving the pre-filter numbers behind.
fn renumber_siblings(rows: &mut [TreeRow]) {
    let mut parents = vec![None; rows.len()];
    let mut open: Vec<(u8, usize)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        while open.last().is_some_and(|(depth, _)| *depth >= row.depth) {
            open.pop();
        }
        parents[index] = open.last().map(|(_, parent)| *parent);
        open.push((row.depth, index));
    }

    let mut sizes: HashMap<Option<usize>, usize> = HashMap::new();
    for parent in &parents {
        *sizes.entry(*parent).or_default() += 1;
    }
    let mut positions: HashMap<Option<usize>, usize> = HashMap::new();
    for (index, parent) in parents.iter().enumerate() {
        let size = sizes[parent];
        let position = positions.entry(*parent).or_default();
        *position += 1;
        rows[index].pos_in_set = *position;
        rows[index].set_size = size;
    }
}

struct KindNode {
    label: SharedString,
    kind: SharedString,
    count: Option<usize>,
    entry: Option<ResourceEntry>,
    gvk: Option<ResourceIdentity>,
}

impl KindNode {
    fn row(&self, cluster: &str, group: &GroupNode, depth: u8) -> TreeRow {
        let id = self.entry.as_ref().map_or_else(
            || kind_id(cluster, &group.name, &self.kind),
            |entry| resource_entry_id(cluster, entry),
        );
        TreeRow {
            id,
            label: self.label.clone(),
            detail: self.count.map(|count| design::format::count(count).into()),
            depth,
            kind: TreeRowKind::Kind,
            expanded: false,
            pos_in_set: 0,
            set_size: 0,
            resource_kind: Some(self.kind.clone()),
            resource_gvk: self.gvk.clone(),
            entry: self.entry.clone(),
        }
    }
}

struct GroupNode {
    name: SharedString,
    label: SharedString,
    kinds: Vec<KindNode>,
}

impl GroupNode {
    fn count(&self) -> Option<usize> {
        self.kinds
            .iter()
            .try_fold(0usize, |total, kind| Some(total + kind.count?))
    }
}

struct ClusterNode {
    name: SharedString,
    groups: Vec<GroupNode>,
}

/// The core API group reports itself as `core`. It keeps its own row so its
/// Kinds stay above the API-group container.
fn is_core_group(group: &GroupNode) -> bool {
    group.name == CORE_GROUP
}

impl ClusterNode {
    fn count(&self) -> Option<usize> {
        self.groups
            .iter()
            .try_fold(0usize, |total, group| Some(total + group.count()?))
    }

    /// The groups the API-group container holds, in catalog order.
    fn api_groups(&self) -> impl Iterator<Item = &GroupNode> {
        self.groups.iter().filter(|group| !is_core_group(group))
    }

    /// Number of API groups the container would hold.
    fn api_group_count(&self) -> usize {
        self.api_groups().count()
    }

    /// Builds the group rows and, for every expanded group, its Kind rows.
    fn push_group_rows(
        &self,
        rows: &mut Vec<TreeRow>,
        groups: &[&GroupNode],
        depth: u8,
        collapsed: &HashSet<SharedString>,
    ) {
        for group in groups {
            let id = group_id(&self.name, &group.name);
            let expanded = !collapsed.contains(&id);
            rows.push(TreeRow {
                id,
                label: group.label.clone(),
                detail: group
                    .count()
                    .map(|count| design::format::count(count).into()),
                depth,
                kind: TreeRowKind::Group,
                expanded,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: None,
                resource_gvk: None,
                entry: None,
            });
            if !expanded {
                continue;
            }
            rows.extend(
                group
                    .kinds
                    .iter()
                    .map(|kind| kind.row(&self.name, group, depth + 1)),
            );
        }
    }
}

pub struct ResourceTree {
    clusters: Vec<ClusterNode>,
}

fn kind(label: &str, kind: &str, count: usize) -> KindNode {
    KindNode {
        label: SharedString::from(label),
        kind: SharedString::from(kind),
        count: Some(count),
        entry: None,
        gvk: None,
    }
}

fn group(name: &str, kinds: Vec<KindNode>) -> GroupNode {
    let label = name.rsplit_once('/').map_or(name, |(group, _)| group);
    GroupNode {
        name: SharedString::from(name),
        label: SharedString::from(label),
        kinds,
    }
}

fn cluster(name: &str, groups: Vec<GroupNode>) -> ClusterNode {
    ClusterNode {
        name: SharedString::from(name),
        groups,
    }
}

pub fn cluster_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}"))
}

pub fn group_id(cluster: &str, group: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/group/{group}"))
}

/// Id of the container row that holds the non-core API groups.
///
/// The id is derived from the cluster name, so an expanded container keeps its
/// state when a catalog refresh rebuilds the tree.
pub fn api_groups_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/api-groups"))
}

pub fn kind_id(cluster: &str, group: &str, kind: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/group/{group}/kind/{kind}"))
}

pub fn resource_id(cluster: &str, group: &str, version: &str, kind: &str) -> SharedString {
    SharedString::from(format!(
        "cluster/{cluster}/group/{group}/version/{version}/kind/{kind}"
    ))
}

pub fn resource_entry_id(cluster: &str, entry: &ResourceEntry) -> SharedString {
    SharedString::from(format!(
        "{}/resource/{}",
        resource_id(cluster, &entry.group, &entry.version, &entry.kind),
        entry.plural
    ))
}

/// Adds spaces at camel-case boundaries while preserving consecutive capitals.
fn humanize_kind(kind: &str) -> String {
    let mut out = String::with_capacity(kind.len() + 4);
    let mut previous: Option<char> = None;
    for ch in kind.chars() {
        if let Some(previous) = previous
            && ch.is_uppercase()
            && (previous.is_lowercase() || previous.is_ascii_digit())
        {
            out.push(' ');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

/// Applies regular English plural rules. The caller handles irregular API plurals.
fn pluralize_word(word: &str) -> String {
    let lower = word.to_lowercase();
    if lower.ends_with('s')
        || lower.ends_with('x')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        format!("{word}es")
    } else if lower.ends_with('y')
        && !["ay", "ey", "oy", "uy"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    {
        format!("{}ies", &word[..word.len() - 1])
    } else {
        format!("{word}s")
    }
}

fn pluralized_kind_label(humanized: &str) -> String {
    let (head, last) = humanized.rsplit_once(' ').unwrap_or(("", humanized));
    if head.is_empty() {
        pluralize_word(last)
    } else {
        format!("{head} {}", pluralize_word(last))
    }
}

/// Kinds whose API name is not what a reader calls the thing.
///
/// Camel-case splitting cannot read an initialism (`APIService` pluralizes to
/// `APIServices`, which reads as a typo) and cannot know that `CSINode` is the
/// API the kubectl output calls `componentstatuses`. Both surfaces that show a
/// kind — the tree and the kind picker, which both read this label — have to
/// agree, and a raw Kind leaking into one of them is how they drift.
const KIND_LABELS: &[(&str, &str)] = &[
    ("APIService", "API Services"),
    ("CSIDriver", "CSI Drivers"),
    ("CSINode", "Component Statuses"),
];

/// Uses the API plural when it matches regular English rules. Otherwise, it humanizes the kind.
fn display_label(kind: &str, plural: &str) -> SharedString {
    if let Some((_, label)) = KIND_LABELS.iter().find(|(known, _)| *known == kind) {
        return SharedString::from(*label);
    }
    let humanized = humanize_kind(kind);
    let expected = pluralized_kind_label(&humanized);
    if expected.replace(' ', "").eq_ignore_ascii_case(plural) {
        SharedString::from(expected)
    } else {
        SharedString::from(humanized)
    }
}

fn parent_index(rows: &[TreeRow], index: usize) -> Option<usize> {
    let parent_depth = rows.get(index)?.depth.checked_sub(1)?;
    (0..index).rev().find(|candidate| {
        rows[*candidate].depth == parent_depth
            && !matches!(rows[*candidate].kind, TreeRowKind::Overview)
    })
}

impl ResourceTree {
    /// Creates a placeholder cluster while the catalog loads.
    pub fn empty(cluster_name: &str) -> Self {
        Self {
            clusters: vec![ClusterNode {
                name: SharedString::from(cluster_name),
                groups: Vec::new(),
            }],
        }
    }

    /// Builds cluster, group, and kind rows from a discovery catalog.
    /// Keeps one entry for each kind in version preference order.
    pub fn from_catalog(catalog: &ResourceCatalog, cluster_name: &str) -> Self {
        let mut groups = Vec::with_capacity(catalog.groups().len());
        for group in catalog.groups() {
            let mut versions: Vec<_> = group.versions.iter().collect();
            versions.sort_by(|left, right| {
                let left_preferred =
                    group.preferred_version.as_deref() == Some(left.version.as_str());
                let right_preferred =
                    group.preferred_version.as_deref() == Some(right.version.as_str());
                right_preferred
                    .cmp(&left_preferred)
                    .then_with(|| {
                        Reverse(Version::parse(left.version.as_str()).priority())
                            .cmp(&Reverse(Version::parse(right.version.as_str()).priority()))
                    })
                    .then_with(|| left.version.cmp(&right.version))
            });

            let mut seen_kinds = HashSet::new();
            let mut kinds = Vec::new();
            for version in versions {
                for entry in &version.resources {
                    if !seen_kinds.insert(entry.kind.clone()) {
                        continue;
                    }
                    kinds.push(KindNode {
                        label: display_label(&entry.kind, &entry.plural),
                        kind: SharedString::from(entry.kind.clone()),
                        count: None,
                        entry: Some(entry.clone()),
                        gvk: Some(entry.identity()),
                    });
                }
            }
            groups.push(GroupNode {
                name: SharedString::from(group.display_name()),
                label: SharedString::from(group.display_name()),
                kinds,
            });
        }
        groups.sort_by(|left, right| {
            let left_name = if left.name.as_ref() == "core" {
                ""
            } else {
                left.name.as_ref()
            };
            let right_name = if right.name.as_ref() == "core" {
                ""
            } else {
                right.name.as_ref()
            };
            left_name.cmp(right_name)
        });

        // A Kind row only needs a disambiguator when two rows *in the same group*
        // would otherwise read the same. The group row directly above already
        // names the group, so a label that repeats across groups needs nothing:
        // counting the whole catalog marked both the core `ComponentStatus` and
        // `storage.k8s.io`'s `CSINode` ambiguous, and the second one grew to
        // `Component Statuses · storage.k8s.io/v1`, which the 232px sidebar cut
        // mid-glyph with no ellipsis. Only the kind can differ inside one group.
        for group in &mut groups {
            let mut label_counts = HashMap::<String, usize>::new();
            for kind in &group.kinds {
                *label_counts.entry(kind.label.to_string()).or_default() += 1;
            }
            for kind in &mut group.kinds {
                if label_counts
                    .get(&kind.label.to_string())
                    .copied()
                    .unwrap_or_default()
                    > 1
                    && let Some(entry) = &kind.entry
                {
                    kind.label = SharedString::from(format!("{} · {}", kind.label, entry.kind));
                }
            }
        }

        Self {
            clusters: vec![ClusterNode {
                name: SharedString::from(cluster_name),
                groups,
            }],
        }
    }

    pub fn demo() -> Self {
        Self {
            clusters: vec![
                cluster(
                    "kind-k8s-gpui-dev",
                    vec![
                        group(
                            "core/v1",
                            vec![
                                kind("Pods", "Pod", 10_000),
                                kind("Services", "Service", 12),
                                kind("Config Maps", "ConfigMap", 48),
                                kind("Secrets", "Secret", 31),
                                kind("Nodes", "Node", 1),
                                kind("Namespaces", "Namespace", 9),
                            ],
                        ),
                        group(
                            "apps/v1",
                            vec![
                                kind("Deployments", "Deployment", 214),
                                kind("Stateful Sets", "StatefulSet", 6),
                                kind("Daemon Sets", "DaemonSet", 4),
                                kind("Replica Sets", "ReplicaSet", 231),
                            ],
                        ),
                        group(
                            "batch/v1",
                            vec![kind("Jobs", "Job", 12), kind("Cron Jobs", "CronJob", 5)],
                        ),
                        group(
                            "networking.k8s.io/v1",
                            vec![
                                kind("Ingresses", "Ingress", 8),
                                kind("Network Policies", "NetworkPolicy", 3),
                            ],
                        ),
                    ],
                ),
                cluster(
                    "prod-eu-1",
                    vec![
                        group(
                            "core/v1",
                            vec![
                                kind("Pods", "Pod", 342),
                                kind("Services", "Service", 57),
                                kind("Config Maps", "ConfigMap", 96),
                                kind("Secrets", "Secret", 88),
                                kind("Nodes", "Node", 6),
                            ],
                        ),
                        group(
                            "apps/v1",
                            vec![
                                kind("Deployments", "Deployment", 41),
                                kind("Stateful Sets", "StatefulSet", 9),
                                kind("Daemon Sets", "DaemonSet", 6),
                            ],
                        ),
                        group(
                            "cert-manager.io/v1",
                            vec![
                                kind("Certificates", "Certificate", 18),
                                kind("Issuers", "Issuer", 2),
                            ],
                        ),
                        group(
                            "monitoring.coreos.com/v1",
                            vec![
                                kind("Prometheuses", "Prometheus", 1),
                                kind("Service Monitors", "ServiceMonitor", 26),
                            ],
                        ),
                    ],
                ),
            ],
        }
    }

    pub fn cluster_names(&self) -> Vec<SharedString> {
        self.clusters
            .iter()
            .map(|cluster| cluster.name.clone())
            .collect()
    }

    /// Returns the number of Kind rows across all clusters.
    pub fn kind_count(&self) -> usize {
        self.clusters
            .iter()
            .flat_map(|cluster| &cluster.groups)
            .map(|group| group.kinds.len())
            .sum()
    }

    /// Expands the first cluster and its first group. Other groups, and the
    /// API-group container, stay collapsed so a long group list cannot bury the
    /// Kinds above it.
    pub fn default_collapsed(&self) -> HashSet<SharedString> {
        let mut collapsed = HashSet::new();
        for (cluster_index, cluster) in self.clusters.iter().enumerate() {
            if cluster.api_group_count() > API_GROUPS_INLINE_LIMIT {
                collapsed.insert(api_groups_id(&cluster.name));
            }
            for (group_index, group) in cluster.groups.iter().enumerate() {
                if cluster_index == 0 && group_index == 0 {
                    continue;
                }
                collapsed.insert(group_id(&cluster.name, &group.name));
            }
        }
        collapsed
    }

    pub fn rows(&self, collapsed: &HashSet<SharedString>) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for cluster in &self.clusters {
            let id = cluster_id(&cluster.name);
            let expanded = !collapsed.contains(&id);
            rows.push(TreeRow {
                id,
                label: cluster.name.clone(),
                detail: cluster
                    .count()
                    .map(|count| design::format::count(count).into()),
                depth: 0,
                kind: TreeRowKind::Cluster,
                expanded,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: None,
                resource_gvk: None,
                entry: None,
            });
            if !expanded {
                continue;
            }
            // The core group keeps its own row, so its Kinds stay visible; the
            // remaining groups sit behind one container that reports how many
            // API groups it holds.
            let core: Vec<&GroupNode> = cluster
                .groups
                .iter()
                .filter(|group| is_core_group(group))
                .collect();
            let api_groups: Vec<&GroupNode> = cluster.api_groups().collect();
            cluster.push_group_rows(&mut rows, &core, 1, collapsed);
            if api_groups.is_empty() {
                continue;
            }
            if api_groups.len() > API_GROUPS_INLINE_LIMIT {
                let id = api_groups_id(&cluster.name);
                let expanded = !collapsed.contains(&id);
                rows.push(TreeRow {
                    id,
                    label: SharedString::from(API_GROUPS_LABEL),
                    detail: Some(design::format::count(api_groups.len()).into()),
                    depth: 1,
                    kind: TreeRowKind::Group,
                    expanded,
                    pos_in_set: 0,
                    set_size: 0,
                    resource_kind: None,
                    resource_gvk: None,
                    entry: None,
                });
                if expanded {
                    cluster.push_group_rows(&mut rows, &api_groups, 2, collapsed);
                }
            } else {
                cluster.push_group_rows(&mut rows, &api_groups, 1, collapsed);
            }
        }
        renumber_siblings(&mut rows);
        rows
    }

    pub fn rows_for_cluster(
        &self,
        cluster_name: &str,
        collapsed: &HashSet<SharedString>,
    ) -> Vec<TreeRow> {
        if self.kind_count() == 0 {
            return Vec::new();
        }
        let cluster_id = cluster_id(cluster_name);
        let cluster_prefix = format!("{cluster_id}/");
        let mut rows: Vec<TreeRow> = self
            .rows(collapsed)
            .into_iter()
            .filter_map(|mut row| {
                let id = row.id.as_ref();
                if id == cluster_id.as_ref() {
                    return None;
                }
                if !id.starts_with(&cluster_prefix) {
                    return None;
                }
                row.depth = row.depth.saturating_sub(1);
                Some(row)
            })
            .collect();
        // Keep Overview first and reuse resource_kind for tab selection.
        rows.insert(
            0,
            TreeRow {
                id: SharedString::from(format!("{cluster_id}/overview")),
                label: SharedString::from("Overview"),
                detail: None,
                depth: 0,
                kind: TreeRowKind::Overview,
                expanded: false,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: Some(SharedString::from("Overview")),
                resource_gvk: None,
                entry: None,
            },
        );
        renumber_siblings(&mut rows);
        rows
    }

    pub fn rows_for_cluster_filtered(
        &self,
        cluster_name: &str,
        collapsed: &HashSet<SharedString>,
        query: &str,
    ) -> Vec<TreeRow> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return self.rows_for_cluster(cluster_name, collapsed);
        }
        if self.kind_count() == 0 {
            return Vec::new();
        }

        let cluster_id = cluster_id(cluster_name);
        let cluster_prefix = format!("{cluster_id}/");
        let original_rows = self.rows(collapsed);
        let expanded_by_id = original_rows
            .iter()
            .map(|row| (row.id.clone(), row.expanded))
            .collect::<HashMap<_, _>>();
        let mut rows = self
            .rows(&HashSet::new())
            .into_iter()
            .filter(|row| row.id == cluster_id || row.id.as_ref().starts_with(&cluster_prefix))
            .map(|mut row| {
                if let Some(expanded) = expanded_by_id.get(&row.id) {
                    row.expanded = *expanded;
                }
                row
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Vec::new();
        }
        rows.insert(
            0,
            TreeRow {
                id: SharedString::from(format!("{cluster_id}/overview")),
                label: SharedString::from("Overview"),
                detail: None,
                depth: 0,
                kind: TreeRowKind::Overview,
                expanded: false,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: Some(SharedString::from("Overview")),
                resource_gvk: None,
                entry: None,
            },
        );

        let ranked = fuzzy::rank(query.as_str(), rows.iter().map(|row| row.label.as_ref()));
        if ranked.is_empty() {
            return Vec::new();
        }
        let mut included = vec![false; rows.len()];
        let mut ancestors = vec![false; rows.len()];
        for ranked in ranked {
            let mut index = ranked.index;
            loop {
                included[index] = true;
                if rows[index].depth == 0 {
                    break;
                }
                let Some(parent) = parent_index(&rows, index) else {
                    break;
                };
                ancestors[parent] = true;
                index = parent;
            }
        }

        let mut rows: Vec<TreeRow> = rows
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut row)| {
                if !included[index] || row.kind == TreeRowKind::Cluster {
                    return None;
                }
                if ancestors[index] {
                    row.expanded = true;
                }
                row.depth = row.depth.saturating_sub(1);
                Some(row)
            })
            .collect();
        // The filter removed siblings, so the set numbers have to be reported again.
        renumber_siblings(&mut rows);
        rows
    }
}

#[cfg(test)]
mod tests {
    use kube_core::GroupVersionKind;
    use serde_json::json;

    use super::*;

    fn labels(rows: &[TreeRow]) -> Vec<String> {
        rows.iter().map(|row| row.label.to_string()).collect()
    }

    /// Builds a catalog through the public serde shape.
    fn sample_catalog() -> ResourceCatalog {
        serde_json::from_value(json!({
            "groups": [
                {
                    "group": "",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                              "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                            { "group": "", "version": "v1", "kind": "Node", "plural": "nodes",
                              "scope": "cluster", "verbs": ["get", "list"] },
                        ],
                    }],
                },
                {
                    "group": "apps",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "apps", "version": "v1", "kind": "Deployment", "plural": "deployments",
                              "scope": "namespaced", "verbs": ["get", "list"] },
                        ],
                    }],
                },
                {
                    "group": "networking.k8s.io",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "networking.k8s.io", "version": "v1", "kind": "NetworkPolicy",
                              "plural": "networkpolicies", "scope": "namespaced", "verbs": ["get", "list"] },
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON")
    }

    /// Builds a catalog with the core group plus `groups` extra API groups.
    fn catalog_with_api_groups(groups: usize) -> ResourceCatalog {
        let mut list = vec![json!({
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                      "scope": "namespaced", "verbs": ["list"] },
                ],
            }],
        })];
        for index in 0..groups {
            let group = format!("g{index}.example");
            list.push(json!({
                "group": group,
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": group, "version": "v1", "kind": format!("Widget{index}"),
                          "plural": format!("widget{index}s"), "scope": "namespaced",
                          "verbs": ["list"] },
                    ],
                }],
            }));
        }
        serde_json::from_value(json!({ "groups": list })).expect("valid catalog JSON")
    }

    #[test]
    fn catalog_tree_lists_cluster_group_and_kind() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "kind-k8s-gpui-dev");
        assert_eq!(tree.cluster_names(), vec!["kind-k8s-gpui-dev"]);
        assert_eq!(tree.kind_count(), 4);

        let rows = tree.rows(&HashSet::new());
        assert_eq!(
            labels(&rows),
            vec![
                "kind-k8s-gpui-dev",
                "core",
                "Pods",
                "Nodes",
                "apps",
                "Deployments",
                "networking.k8s.io",
                "Network Policies",
            ]
        );
        // The contract is a gap, not a rule. `DESIGN.md` §4 lists sidebar rows as a
        // Health Rail landing point and §9 admits the data is not plumbed:
        // `from_catalog` writes `count: None` for every kind, and `TreeRow` has no
        // health or confidence field at all, so no assertion can be written about
        // a marker that does not exist in the type. The old failure message read
        // "catalog trees do not show counts", which told whoever adds them that
        // they were wrong. When the counts land, delete the first assertion.
        assert!(
            rows.iter().all(|row| row.detail.is_none()),
            "catalog rows carry no live counts yet: the data is not plumbed (DESIGN.md §4, §9)"
        );
        // `detail` is the only status channel a row has today, and the demo tree
        // fills it. So an empty sidebar on a real cluster is a missing data path,
        // not a cluster with nothing in it.
        let demo_rows = ResourceTree::demo().rows(&HashSet::new());
        assert!(
            demo_rows.iter().any(|row| row.detail.is_some()),
            "a row can carry a count; the catalog path just has none to carry"
        );
        let pods = rows
            .iter()
            .find(|row| row.label == "Pods")
            .expect("Pods row");
        assert_eq!(pods.resource_kind.as_deref(), Some("Pod"));
        assert_eq!(
            pods.resource_gvk.clone(),
            Some(GroupVersionKind::gvk("", "v1", "Pod"))
        );
        assert_eq!(pods.depth, 2);
        assert_eq!(pods.kind, TreeRowKind::Kind);
        let entry = pods
            .entry
            .as_ref()
            .expect("catalog row must include ResourceEntry");
        assert_eq!(entry.plural, "pods");
        assert_eq!(entry.api_version(), "v1");
        assert!(entry.namespaced());

        let demo = ResourceTree::demo();
        assert!(
            demo.rows(&HashSet::new())
                .iter()
                .all(|row| row.entry.is_none()),
            "demo tree has no catalog entries"
        );
    }

    /// The same kind in two API groups, which is the `ComponentStatus` /
    /// `CSINode` shape: both humanize to one label.
    ///
    /// The group row directly above each Kind row already names the group, so the
    /// rows do not repeat it. They used to read `Widgets · a.example/v1`, and in
    /// the real catalog the `storage.k8s.io` one read
    /// `Component Statuses · storage.k8s.io/v1` — 32 characters in a 232px
    /// sidebar, cut mid-glyph with no ellipsis.
    #[test]
    fn same_kind_rows_keep_gvk_identity_and_readable_labels() {
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [
                {
                    "group": "z.example",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "z.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                              "scope": "namespaced", "verbs": ["list"] }
                        ],
                    }],
                },
                {
                    "group": "a.example",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "a.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                              "scope": "cluster", "verbs": ["list"] }
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON");
        let tree = ResourceTree::from_catalog(&catalog, "c");
        let rows = tree.rows(&HashSet::new());
        let widgets: Vec<&TreeRow> = rows
            .iter()
            .filter(|row| row.resource_kind.as_deref() == Some("Widget"))
            .collect();
        assert_eq!(
            widgets
                .iter()
                .map(|row| row.label.to_string())
                .collect::<Vec<_>>(),
            ["Widgets", "Widgets"]
        );
        // Identity is still exact, and the label no longer carries a second
        // naming scheme next to the group row that already names the group.
        assert_eq!(
            widgets
                .iter()
                .map(|row| row.label.chars().count())
                .collect::<Vec<_>>(),
            [7, 7]
        );
        assert_eq!(
            widgets[0].resource_gvk.clone(),
            Some(GroupVersionKind::gvk("a.example", "v1", "Widget"))
        );
        assert_ne!(
            widgets[0].id, widgets[1].id,
            "one label, two groups, two rows: the id carries the group"
        );
        assert_eq!(
            widgets[0].id,
            resource_entry_id("c", widgets[0].entry.as_ref().unwrap())
        );
        let filtered = tree.rows_for_cluster_filtered("c", &HashSet::new(), "widget");
        assert_eq!(
            labels(&filtered),
            vec!["a.example", "Widgets", "z.example", "Widgets"]
        );
        assert_eq!(
            filtered
                .iter()
                .filter(|row| row.resource_kind.as_deref() == Some("Widget"))
                .count(),
            2
        );
    }

    /// Two kinds in one group that humanize to the same word, which is the only
    /// case a Kind row still needs a disambiguator for — the group row above it
    /// already names the group, so the kind is what has to differ.
    #[test]
    fn a_label_that_collides_inside_one_group_names_the_kind() {
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [
                {
                    "group": "",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "", "version": "v1", "kind": "Policy", "plural": "policies",
                              "scope": "namespaced", "verbs": ["list"] },
                            { "group": "", "version": "v1", "kind": "Policies", "plural": "policys",
                              "scope": "namespaced", "verbs": ["list"] },
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON");
        let rows = ResourceTree::from_catalog(&catalog, "c").rows(&HashSet::new());
        assert_eq!(
            labels(&rows),
            vec!["c", "core", "Policies · Policy", "Policies · Policies"]
        );
    }

    #[test]
    fn demo_rows_use_human_labels_and_keep_group_identity() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let group = rows
            .iter()
            .find(|row| row.kind == TreeRowKind::Group && row.label == "networking.k8s.io")
            .expect("networking group");
        assert_eq!(
            group.id,
            group_id("kind-k8s-gpui-dev", "networking.k8s.io/v1")
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == TreeRowKind::Kind && row.label == "Network Policies")
        );
    }

    #[test]
    fn catalog_default_collapses_every_group_except_core() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let rows = tree.rows(&tree.default_collapsed());
        assert_eq!(
            labels(&rows),
            vec!["c", "core", "Pods", "Nodes", "apps", "networking.k8s.io"],
            "only the first group is expanded by default"
        );
    }

    #[test]
    fn display_label_humanizes_kind_and_plural() {
        assert_eq!(display_label("Pod", "pods"), "Pods");
        assert_eq!(display_label("Ingress", "ingresses"), "Ingresses");
        assert_eq!(display_label("ConfigMap", "configmaps"), "Config Maps");
        assert_eq!(
            display_label("NetworkPolicy", "networkpolicies"),
            "Network Policies"
        );
        assert_eq!(
            display_label("CustomResourceDefinition", "customresourcedefinitions"),
            "Custom Resource Definitions"
        );
        assert_eq!(display_label("Endpoints", "endpoints"), "Endpoints");
    }

    /// A raw Kind must not reach the reader, and the two surfaces that show one
    /// have to spell it the same way.
    #[test]
    fn kinds_the_api_does_not_name_readably_use_the_shared_label() {
        for (kind, label) in KIND_LABELS {
            assert!(label.contains(' '), "{label} is not a readable phrase");
            assert_eq!(display_label(kind, "").as_ref(), *label);
        }
        assert_eq!(display_label("APIService", "apiservices"), "API Services");
        assert_eq!(display_label("CSINode", "csinodes"), "Component Statuses");
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [{
                "group": "storage.k8s.io",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "storage.k8s.io", "version": "v1", "kind": "CSINode",
                          "plural": "csinodes", "scope": "cluster", "verbs": ["list"] },
                        { "group": "storage.k8s.io", "version": "v1", "kind": "APIService",
                          "plural": "apiservices", "scope": "cluster", "verbs": ["list"] },
                    ],
                }],
            }],
        }))
        .expect("valid catalog JSON");
        let labels = ResourceTree::from_catalog(&catalog, "c")
            .rows(&HashSet::new())
            .iter()
            .map(|row| row.label.to_string())
            .collect::<Vec<_>>();
        assert!(
            labels.contains(&"Component Statuses".to_owned()),
            "{labels:?}"
        );
        assert!(labels.contains(&"API Services".to_owned()), "{labels:?}");
    }

    #[test]
    fn collapsed_cluster_hides_its_groups_and_kinds() {
        let tree = ResourceTree::demo();
        let collapsed: HashSet<SharedString> =
            [cluster_id("kind-k8s-gpui-dev"), cluster_id("prod-eu-1")].into();
        let rows = tree.rows(&collapsed);
        assert_eq!(
            labels(&rows),
            vec!["kind-k8s-gpui-dev", "prod-eu-1"],
            "collapsed clusters do not show groups"
        );
        assert!(!rows[0].expanded);
    }

    #[test]
    fn collapsing_one_cluster_keeps_the_other_expanded() {
        let tree = ResourceTree::demo();
        let collapsed: HashSet<SharedString> = [cluster_id("kind-k8s-gpui-dev")].into();
        let rows = tree.rows(&collapsed);
        assert!(
            !rows
                .iter()
                .any(|row| row.id.starts_with("cluster/kind-k8s-gpui-dev/group/"))
        );
        assert!(
            rows.iter()
                .any(|row| row.id == kind_id("prod-eu-1", "core/v1", "Pod"))
        );
    }

    #[test]
    fn cluster_projection_has_two_navigation_levels() {
        let tree = ResourceTree::demo();
        let rows = tree.rows_for_cluster("kind-k8s-gpui-dev", &tree.default_collapsed());
        assert!(rows.iter().all(|row| row.depth <= 1));
        assert!(rows.iter().all(|row| row.kind != TreeRowKind::Cluster));
        assert!(rows.iter().any(|row| row.kind == TreeRowKind::Kind));
    }

    #[test]
    fn default_view_shows_three_levels_for_first_group_only() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&tree.default_collapsed());
        assert!(rows.iter().any(|row| row.kind == TreeRowKind::Kind));
        assert!(
            rows.iter().all(|row| row.depth < 2
                || row
                    .id
                    .starts_with("cluster/kind-k8s-gpui-dev/group/core/v1/")),
            "only the first group of the first cluster expands to Kind rows"
        );
        assert_eq!(rows.iter().filter(|row| row.depth == 0).count(), 2);
    }

    #[test]
    fn fully_expanded_tree_has_one_row_per_node() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TreeRowKind::Kind)
                .count(),
            26
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TreeRowKind::Group)
                .count(),
            8
        );
        assert!(
            rows.iter()
                .all(|row| row.expandable() == (row.kind != TreeRowKind::Kind))
        );
        assert!(rows.iter().all(|row| row.detail.is_some()));
    }

    #[test]
    fn parent_rows_show_aggregate_counts() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let detail = |id: SharedString| {
            rows.iter()
                .find(|row| row.id == id)
                .and_then(|row| row.detail.clone())
        };
        assert_eq!(
            detail(group_id("kind-k8s-gpui-dev", "core/v1")),
            Some(SharedString::from("10,101")),
            "counts use the shared thousands separator"
        );
        assert_eq!(
            detail(cluster_id("kind-k8s-gpui-dev")),
            Some(SharedString::from("10,584"))
        );
        assert_eq!(tree.kind_count(), 26);
    }

    #[test]
    fn filtered_cluster_rows_keep_ancestors_and_restore_on_clear() {
        let tree = ResourceTree::demo();
        let collapsed = tree.default_collapsed();
        let original = tree.rows_for_cluster("kind-k8s-gpui-dev", &collapsed);
        let filtered = tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "netpol");
        assert_eq!(
            labels(&filtered),
            vec!["networking.k8s.io", "Network Policies"]
        );
        assert!(
            filtered
                .iter()
                .find(|row| row.label == "networking.k8s.io")
                .expect("matched group")
                .expanded
        );
        assert!(
            tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "zzzz")
                .is_empty()
        );
        let restored = tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "");
        assert_eq!(labels(&restored), labels(&original));
        assert_eq!(
            restored
                .iter()
                .map(|row| (row.id.clone(), row.expanded))
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|row| (row.id.clone(), row.expanded))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn kind_ids_are_unique() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let unique: HashSet<_> = rows.iter().map(|row| row.id.clone()).collect();
        assert_eq!(unique.len(), rows.len());
    }

    #[test]
    fn every_row_reports_its_sibling_set() {
        let tree = ResourceTree::demo();
        let collapsed = tree.default_collapsed();
        for rows in [
            tree.rows(&HashSet::new()),
            tree.rows_for_cluster("kind-k8s-gpui-dev", &collapsed),
            tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "pod"),
        ] {
            assert!(!rows.is_empty());
            assert_reports_parent_sets(&rows);
        }
        let many = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        assert_reports_parent_sets(&many.rows(&many.default_collapsed()));
        assert_reports_parent_sets(&many.rows(&HashSet::new()));
    }

    /// Checks the reported set against the parent/child structure of a
    /// flattened list, where a parent sits directly above its first child.
    fn assert_reports_parent_sets(rows: &[TreeRow]) {
        let mut parents = vec![None; rows.len()];
        let mut open: Vec<(u8, usize)> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            while open.last().is_some_and(|(depth, _)| *depth >= row.depth) {
                open.pop();
            }
            parents[index] = open.last().map(|(_, parent)| *parent);
            open.push((row.depth, index));
        }
        let mut sets: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        for (index, parent) in parents.iter().enumerate() {
            sets.entry(*parent).or_default().push(index);
        }
        for members in sets.values() {
            let size = members.len();
            for (position, index) in members.iter().enumerate() {
                let row = &rows[*index];
                assert_eq!(row.pos_in_set, position + 1, "{}", row.label);
                assert_eq!(row.set_size, size, "{}", row.label);
            }
        }
    }

    #[test]
    fn many_api_groups_collapse_into_one_container_row() {
        let tree = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        let collapsed = tree.default_collapsed();
        assert!(
            collapsed.contains(&api_groups_id("c")),
            "a long API group list starts collapsed"
        );

        let rows = tree.rows_for_cluster("c", &collapsed);
        assert_eq!(
            labels(&rows),
            vec!["Overview", "core", "Pods", API_GROUPS_LABEL],
            "the pinned Kinds stay visible while the API groups hide"
        );
        let container = rows.last().expect("container row");
        assert!(!container.expanded);
        assert!(container.expandable(), "the container is a disclosure row");
        assert_eq!(container.depth, 0);
        assert_eq!(
            container.detail,
            Some(SharedString::from("9")),
            "the container reports how many API groups it holds"
        );
        assert!(
            container.resource_kind.is_none(),
            "a container opens no tab"
        );
        assert!(container.resource_gvk.is_none());
        assert!(container.entry.is_none());

        // The core group and the container are siblings, so they report a
        // two-item set even though the core Kinds sit between the two rows.
        let all = tree.rows(&collapsed);
        let set = |id: SharedString| {
            all.iter()
                .find(|row| row.id == id)
                .map(|row| (row.pos_in_set, row.set_size))
                .expect("row")
        };
        assert_eq!(set(group_id("c", CORE_GROUP)), (1, 2));
        assert_eq!(set(api_groups_id("c")), (2, 2));

        let mut opened = collapsed.clone();
        opened.remove(&api_groups_id("c"));
        let rows = tree.rows_for_cluster("c", &opened);
        let group = rows
            .iter()
            .position(|row| row.label == "g8.example")
            .expect("last API group");
        assert!(rows[3].expanded, "the container reports its state");
        assert_eq!(rows[group].depth, 1, "groups stay one level in");
        opened.remove(&group_id("c", "g8.example"));
        let rows = tree.rows_for_cluster("c", &opened);
        let kind = rows
            .iter()
            .position(|row| row.label == "Widget8s")
            .expect("kind row");
        assert_eq!(rows[kind].depth, 2, "Kinds stay one level below the group");
    }

    #[test]
    fn a_short_api_group_list_stays_outlined() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let rows = tree.rows_for_cluster("c", &tree.default_collapsed());
        assert!(
            rows.iter().all(|row| row.id != api_groups_id("c")),
            "a list that still fits beside the Kinds keeps its own rows"
        );
        assert_eq!(
            labels(&rows),
            vec![
                "Overview",
                "core",
                "Pods",
                "Nodes",
                "apps",
                "networking.k8s.io"
            ]
        );
    }

    #[test]
    fn the_filter_reaches_a_kind_behind_the_container() {
        let tree = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        let filtered = tree.rows_for_cluster_filtered("c", &tree.default_collapsed(), "widget8");
        assert_eq!(
            labels(&filtered),
            vec![API_GROUPS_LABEL, "g8.example", "Widget8s"],
            "a match keeps the container and the group as ancestors"
        );
        assert!(filtered[0].expanded, "the container opens for its matches");
        assert!(
            filtered[1].expanded,
            "the matched group opens for its Kinds"
        );
        assert!(
            tree.rows_for_cluster_filtered("c", &tree.default_collapsed(), "zzzz")
                .is_empty()
        );
    }
}
