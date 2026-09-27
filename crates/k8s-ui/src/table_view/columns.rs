//! Defines resource table columns and computes cell values without UI state.

use jiff::Timestamp;
use k8s_core::projection::{CellValue, Column, SortKey};
use kube_core::DynamicObject;
use serde_json::Value;

use crate::design;

/// Stands in for a value the cluster never reported.
///
/// An empty cell cannot be told apart from a cell whose value happens to be
/// blank, so a Pod that has not published `status.containerStatuses` yet shows a
/// dash instead. It is the same `Dash` shape `design::health_icon` uses for "no
/// verdict", and the same character the Overview grid uses for a missing number.
const NOT_REPORTED: &str = "\u{2014}";

/// Defines the table header and width for one column.
pub struct ResourceColumn {
    pub title: &'static str,
    pub width: f32,
    /// Uses muted text for secondary columns.
    pub muted: bool,
    /// Right-aligns numeric values.
    pub numeric: bool,
    pub column: Column,
}

fn column(
    id: &'static str,
    title: &'static str,
    width: f32,
    muted: bool,
    numeric: bool,
    projector: impl Fn(&DynamicObject) -> CellValue + Send + Sync + 'static,
) -> ResourceColumn {
    ResourceColumn {
        title,
        width,
        muted,
        numeric,
        column: Column::new(id, projector),
    }
}

/// Returns columns for a resource kind and scope.
/// Cluster-scoped resources omit the Namespace column.
pub fn columns_for(kind: &str, namespaced: bool) -> Vec<ResourceColumn> {
    match known_columns(kind, namespaced) {
        Some(columns) => columns,
        None => fallback_columns(namespaced),
    }
}

/// Returns the columns for a known kind, or `None` for an unknown kind. The
/// table uses this to tell "no rows" apart from "no columns for this kind".
pub fn known_columns(kind: &str, namespaced: bool) -> Option<Vec<ResourceColumn>> {
    let columns = match kind {
        "Pod" => pod_columns(),
        "Deployment" => deployment_columns(namespaced),
        "Service" => service_columns(namespaced),
        "Node" => node_columns(),
        "ConfigMap" | "Secret" => data_columns(namespaced),
        "Job" => job_columns(namespaced),
        "CronJob" => cronjob_columns(namespaced),
        "Event" => event_columns(),
        _ => return None,
    };
    Some(columns)
}

/// Reports whether the kind has a known column layout.
pub fn is_known_kind(kind: &str) -> bool {
    known_columns(kind, true).is_some()
}

pub fn pod_columns() -> Vec<ResourceColumn> {
    // `Created` used to sit between `Age` and `Image` and read the same
    // `metadata.creationTimestamp` as `Age`, costing 200px of the row.
    // `kubectl get pods` shows only `AGE`, so the width goes to the two columns
    // that actually truncate: the name a reader scans for and the node it lands
    // on.
    vec![
        column("name", "Name", 400.0, false, false, name_cell),
        column("namespace", "Namespace", 140.0, true, false, namespace_cell),
        column("status", "Status", 150.0, false, false, status_cell),
        column("ready", "Ready", 80.0, false, true, ready_cell),
        column("restarts", "Restarts", 90.0, false, true, restarts_cell),
        column("age", "Age", 80.0, true, true, age_cell),
        column("image", "Image", 320.0, true, false, image_cell),
        column("ip", "IP", 130.0, true, false, ip_cell),
        column("node", "Node", 320.0, true, false, node_cell),
    ]
}

/// Matches the main columns from `kubectl get deploy`.
fn deployment_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column("ready", "Ready", 80.0, false, true, deployment_ready_cell),
        column("up-to-date", "Up-to-Date", 100.0, true, true, |obj| {
            number_at(&obj.data, "/status/updatedReplicas")
                .map_or_else(CellValue::empty, CellValue::number)
        }),
        column("available", "Available", 90.0, true, true, |obj| {
            number_at(&obj.data, "/status/availableReplicas")
                .map_or_else(CellValue::empty, CellValue::number)
        }),
        age_column(),
    ]);
    columns
}

/// Matches the main columns from `kubectl get svc`.
fn service_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column("type", "Type", 110.0, false, false, service_type_cell),
        column("cluster-ip", "Cluster-IP", 140.0, true, false, |obj| {
            text_cell_at(&obj.data, "/spec/clusterIP")
        }),
        column("ports", "Ports", 180.0, true, false, service_ports_cell),
        age_column(),
    ]);
    columns
}

/// Matches the main columns from `kubectl get nodes`.
fn node_columns() -> Vec<ResourceColumn> {
    vec![
        name_column(),
        column("status", "Status", 110.0, false, false, node_status_cell),
        column("roles", "Roles", 160.0, true, false, node_roles_cell),
        age_column(),
        column("version", "Version", 130.0, true, false, |obj| {
            text_cell_at(&obj.data, "/status/nodeInfo/kubeletVersion")
        }),
    ]
}

/// Shows the key count and age for ConfigMaps and Secrets.
fn data_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column("data", "Data", 80.0, false, true, data_count_cell),
        age_column(),
    ]);
    columns
}

/// Shows completed and desired Job counts, plus age.
fn job_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column(
            "completions",
            "Completions",
            100.0,
            false,
            true,
            job_completions_cell,
        ),
        age_column(),
    ]);
    columns
}

/// Shows the schedule and age for CronJobs.
fn cronjob_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column("schedule", "Schedule", 140.0, false, false, |obj| {
            text_cell_at(&obj.data, "/spec/schedule")
        }),
        age_column(),
    ]);
    columns
}

/// Shows the main fields for Kubernetes Events.
fn event_columns() -> Vec<ResourceColumn> {
    vec![
        column("type", "Type", 100.0, false, false, |obj| {
            text_cell_at(&obj.data, "/type")
        }),
        column("reason", "Reason", 160.0, false, false, |obj| {
            text_cell_at(&obj.data, "/reason")
        }),
        column("object", "Object", 260.0, true, false, event_object_cell),
        column(
            "last-seen",
            "Last Seen",
            100.0,
            true,
            true,
            event_last_seen_cell,
        ),
    ]
}

/// Returns the default Name, Namespace, and Age columns.
fn fallback_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![name_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.push(age_column());
    columns
}

fn name_column() -> ResourceColumn {
    column("name", "Name", 320.0, false, false, name_cell)
}

fn namespace_column() -> ResourceColumn {
    column("namespace", "Namespace", 140.0, true, false, namespace_cell)
}

fn age_column() -> ResourceColumn {
    column("age", "Age", 80.0, true, true, age_cell)
}

fn name_cell(obj: &DynamicObject) -> CellValue {
    obj.metadata
        .name
        .as_deref()
        .map_or_else(CellValue::empty, CellValue::text)
}

fn namespace_cell(obj: &DynamicObject) -> CellValue {
    obj.metadata
        .namespace
        .as_deref()
        .map_or_else(CellValue::empty, CellValue::text)
}

/// Shows a waiting reason before the Pod phase.
fn status_cell(obj: &DynamicObject) -> CellValue {
    let waiting = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
        .and_then(|statuses| {
            statuses
                .iter()
                .find_map(|container| text_at(container, "/state/waiting/reason"))
        });
    waiting
        .or_else(|| text_at(&obj.data, "/status/phase"))
        .map_or_else(CellValue::empty, CellValue::text)
}

fn ready_cell(obj: &DynamicObject) -> CellValue {
    let Some(statuses) = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
    else {
        // The cluster has not reported the containers yet. An empty cell here
        // looked identical to a blank value, so every `Pending` row showed two
        // cells that said nothing.
        return CellValue::text(NOT_REPORTED);
    };
    let ready = statuses
        .iter()
        .filter(|container| {
            container
                .get("ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count() as i64;
    ratio_cell(ready, statuses.len() as i64)
}

fn restarts_cell(obj: &DynamicObject) -> CellValue {
    let Some(statuses) = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
    else {
        return CellValue::text(NOT_REPORTED);
    };
    let total = statuses
        .iter()
        .filter_map(|container| {
            container
                .get("restartCount")
                .and_then(serde_json::Value::as_i64)
        })
        .sum();
    CellValue::number(total)
}

/// Formats a relative age and sorts on the age itself, so the order always
/// follows the displayed text. Clock skew must not create a negative age.
fn age_value(seconds: i64) -> CellValue {
    let age = Timestamp::now().as_second().saturating_sub(seconds).max(0);
    CellValue::new(format_age(age), SortKey::Int(age))
}

/// Formats a relative age, sorting on the age itself so the order always
/// follows the displayed text.
fn age_cell(obj: &DynamicObject) -> CellValue {
    match creation_seconds(obj) {
        Some(created) => age_value(created),
        None => CellValue::empty(),
    }
}

fn creation_seconds(obj: &DynamicObject) -> Option<i64> {
    obj.metadata
        .creation_timestamp
        .as_ref()
        .map(|time| time.0.as_second())
}

/// Shows the first container's image and counts the ones it hides.
///
/// A Pod with a sidecar rendered one image with nothing to say a second
/// container existed, so the column looked complete when it was not.
fn image_cell(obj: &DynamicObject) -> CellValue {
    let Some(image) = text_at(&obj.data, "/spec/containers/0/image") else {
        return CellValue::empty();
    };
    let hidden = obj
        .data
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, |containers| containers.len().saturating_sub(1));
    if hidden == 0 {
        return CellValue::text(image);
    }
    CellValue::text(format!("{image}+{}", design::format::count(hidden)))
}

fn ip_cell(obj: &DynamicObject) -> CellValue {
    text_cell_at(&obj.data, "/status/podIP")
}

fn node_cell(obj: &DynamicObject) -> CellValue {
    text_cell_at(&obj.data, "/spec/nodeName")
}

fn ratio_cell(ready: i64, desired: i64) -> CellValue {
    let display = format!("{ready}/{desired}");
    CellValue::new(display, SortKey::Int(ratio_sort_key(ready, desired)))
}

fn ratio_sort_key(ready: i64, desired: i64) -> i64 {
    if desired <= 0 {
        return if ready <= 0 { 0 } else { 100 };
    }
    (ready.max(0).min(desired).saturating_mul(100)) / desired
}

/// Shows ready replicas over `spec.replicas`, with a default of 1.
fn deployment_ready_cell(obj: &DynamicObject) -> CellValue {
    let desired = number_at(&obj.data, "/spec/replicas");
    let ready = number_at(&obj.data, "/status/readyReplicas");
    if desired.is_none() && ready.is_none() {
        return CellValue::empty();
    }
    let ready = ready.unwrap_or(0);
    let desired = desired.unwrap_or(1);
    ratio_cell(ready, desired)
}

/// Defaults Type to ClusterIP only when `spec` exists.
fn service_type_cell(obj: &DynamicObject) -> CellValue {
    if let Some(kind) = text_at(&obj.data, "/spec/type") {
        return CellValue::text(kind);
    }
    if obj.data.get("spec").is_some() {
        CellValue::text("ClusterIP")
    } else {
        CellValue::empty()
    }
}

/// Formats ports and optional node-port mappings.
fn service_ports_cell(obj: &DynamicObject) -> CellValue {
    let Some(ports) = obj.data.pointer("/spec/ports").and_then(Value::as_array) else {
        return CellValue::empty();
    };
    let rendered: Vec<String> = ports
        .iter()
        .filter_map(|port| {
            let number = port.get("port").and_then(Value::as_i64)?;
            let protocol = port
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("TCP");
            Some(match port.get("nodePort").and_then(Value::as_i64) {
                Some(node_port) => format!("{number}:{node_port}/{protocol}"),
                None => format!("{number}/{protocol}"),
            })
        })
        .collect();
    if rendered.is_empty() {
        CellValue::empty()
    } else {
        CellValue::text(rendered.join(", "))
    }
}

/// Maps the Ready condition to `Ready` or `NotReady`.
fn node_status_cell(obj: &DynamicObject) -> CellValue {
    let status = obj
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions
                .iter()
                .find(|condition| condition.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .and_then(|condition| condition.get("status").and_then(Value::as_str));
    match status {
        Some("True") => CellValue::text("Ready"),
        Some(_) => CellValue::text("NotReady"),
        None => CellValue::empty(),
    }
}

/// Reads roles from `node-role.kubernetes.io/<role>` labels.
fn node_roles_cell(obj: &DynamicObject) -> CellValue {
    const PREFIX: &str = "node-role.kubernetes.io/";
    let Some(labels) = obj.metadata.labels.as_ref() else {
        return CellValue::empty();
    };
    let mut roles: Vec<&str> = labels
        .keys()
        .filter_map(|key| key.strip_prefix(PREFIX))
        .filter(|role| !role.is_empty())
        .collect();
    roles.sort_unstable();
    if roles.is_empty() {
        CellValue::empty()
    } else {
        CellValue::text(roles.join(","))
    }
}

/// Counts keys in `data` and `binaryData`.
fn data_count_cell(obj: &DynamicObject) -> CellValue {
    let data = obj.data.get("data").and_then(Value::as_object);
    let binary = obj.data.get("binaryData").and_then(Value::as_object);
    if data.is_none() && binary.is_none() {
        return CellValue::empty();
    }
    let count = data.map_or(0, |map| map.len()) + binary.map_or(0, |map| map.len());
    CellValue::number(count as i64)
}

/// Shows completed and desired Job counts, with a default of 1.
fn job_completions_cell(obj: &DynamicObject) -> CellValue {
    let desired = number_at(&obj.data, "/spec/completions");
    let succeeded = number_at(&obj.data, "/status/succeeded");
    if desired.is_none() && succeeded.is_none() {
        return CellValue::empty();
    }
    let succeeded = succeeded.unwrap_or(0);
    let desired = desired.unwrap_or(1);
    ratio_cell(succeeded, desired)
}

/// Formats an Event object as `Kind/name`.
fn event_object_cell(obj: &DynamicObject) -> CellValue {
    let Some(involved) = obj.data.pointer("/involvedObject") else {
        return CellValue::empty();
    };
    let kind = involved.get("kind").and_then(Value::as_str);
    let name = involved.get("name").and_then(Value::as_str);
    match (kind, name) {
        (Some(kind), Some(name)) => CellValue::text(format!("{kind}/{name}")),
        (None, Some(name)) => CellValue::text(name),
        _ => CellValue::empty(),
    }
}

/// Uses the Event time, then the object creation time.
fn event_last_seen_cell(obj: &DynamicObject) -> CellValue {
    let seconds = ["/lastTimestamp", "/eventTime", "/series/lastObservedTime"]
        .iter()
        .find_map(|pointer| timestamp_seconds_at(&obj.data, pointer))
        .or_else(|| creation_seconds(obj));
    match seconds {
        Some(seconds) => age_value(seconds),
        None => CellValue::empty(),
    }
}

fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer)?.as_str()
}

fn text_cell_at(value: &Value, pointer: &str) -> CellValue {
    text_at(value, pointer).map_or_else(CellValue::empty, CellValue::text)
}

fn number_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64)
}

fn timestamp_seconds_at(value: &Value, pointer: &str) -> Option<i64> {
    text_at(value, pointer)
        .and_then(|text| text.parse::<Timestamp>().ok())
        .map(|time| time.as_second())
}

fn format_age(seconds: i64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(
        created: &str,
        phase: &str,
        ready: bool,
        restarts: i64,
        waiting: Option<&str>,
    ) -> DynamicObject {
        let state = match waiting {
            Some(reason) => json!({ "waiting": { "reason": reason } }),
            None => json!({ "running": {} }),
        };
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web-0a1b2c3d4e",
                "namespace": "default",
                "uid": "uid-web",
                "creationTimestamp": created,
            },
            "spec": {
                "nodeName": "node-07.cluster.local",
                "containers": [
                    { "name": "web", "image": "registry.k8s.io/web:v1.3.2" }
                ],
            },
            "status": {
                "phase": phase,
                "podIP": "10.244.3.17",
                "containerStatuses": [
                    {
                        "name": "web",
                        "ready": ready,
                        "restartCount": restarts,
                        "state": state,
                    }
                ],
            },
        }))
        .expect("synthetic pod")
    }

    fn object(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("synthetic object")
    }

    fn cell(column: &ResourceColumn, obj: &DynamicObject) -> CellValue {
        column.column.cell(obj)
    }

    fn column_by_id(kind: &str, namespaced: bool, id: &str) -> ResourceColumn {
        columns_for(kind, namespaced)
            .into_iter()
            .find(|column| column.column.id == id)
            .expect("column exists")
    }

    fn titles(kind: &str, namespaced: bool) -> Vec<&'static str> {
        columns_for(kind, namespaced)
            .iter()
            .map(|column| column.title)
            .collect()
    }

    #[test]
    fn columns_have_unique_ids_and_expected_titles() {
        let columns = pod_columns();
        let titles: Vec<&str> = columns.iter().map(|column| column.title).collect();
        assert_eq!(
            titles,
            [
                "Name",
                "Namespace",
                "Status",
                "Ready",
                "Restarts",
                "Age",
                "Image",
                "IP",
                "Node"
            ]
        );
        let ids: std::collections::HashSet<&str> = columns
            .iter()
            .map(|column| column.column.id.as_str())
            .collect();
        assert_eq!(ids.len(), columns.len(), "column IDs must be unique");
        assert!(columns.iter().all(|column| column.width > 0.0));
    }

    // `Age` and `Created` read the same `metadata.creation_timestamp`, and
    // `kubectl get pods` shows only `AGE`, so the row carried the same fact
    // twice and paid 200px for it.
    #[test]
    fn the_pod_table_shows_each_fact_once() {
        let columns = pod_columns();
        let ids: Vec<&str> = columns
            .iter()
            .map(|column| column.column.id.as_str())
            .collect();
        assert!(
            !ids.contains(&"created"),
            "Created repeated the same fact as Age and cost 200px of the row"
        );
        // The freed width went to the two columns that truncate: the name a
        // reader scans for and the node it lands on.
        assert_eq!(column_by_id("Pod", true, "name").width, 400.0);
        assert_eq!(column_by_id("Pod", true, "node").width, 320.0);
        let total: f32 = columns.iter().map(|column| column.width).sum();
        assert_eq!(
            total, 1_710.0,
            "removing a column must not make the row any wider"
        );
    }

    #[test]
    fn known_kinds_have_columns_and_unknown_kinds_fall_back() {
        for kind in [
            "Pod",
            "Deployment",
            "Service",
            "Node",
            "ConfigMap",
            "Secret",
            "Job",
            "CronJob",
            "Event",
        ] {
            assert!(is_known_kind(kind), "{kind} must be a known kind");
            assert!(known_columns(kind, true).is_some(), "{kind}");
            assert_eq!(
                columns_for(kind, true).len(),
                known_columns(kind, true).unwrap().len()
            );
        }
        for kind in ["Widget", "widgets.example.com", "", "pod"] {
            assert!(!is_known_kind(kind), "{kind} must stay unknown");
            assert!(known_columns(kind, true).is_none(), "{kind}");
            // The fallback still renders, so the table is never empty.
            assert_eq!(columns_for(kind, true).len(), fallback_columns(true).len());
        }
    }

    #[test]
    fn numeric_columns_are_marked_for_right_alignment() {
        let columns = pod_columns();
        let numeric: Vec<&str> = columns
            .iter()
            .filter(|column| column.numeric)
            .map(|column| column.column.id.as_str())
            .collect();
        assert_eq!(numeric, ["ready", "restarts", "age"]);
    }

    #[test]
    fn cells_project_pod_fields() {
        let now = Timestamp::now().as_second();
        let created = Timestamp::from_second(now - 2 * 3_600 - 30).expect("valid timestamp");
        let obj = pod(&created.to_string(), "Running", true, 3, None);

        assert_eq!(
            cell(&column_by_id("Pod", true, "name"), &obj),
            CellValue::text("web-0a1b2c3d4e")
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "namespace"), &obj),
            CellValue::text("default")
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "status"), &obj),
            CellValue::text("Running")
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "ready"), &obj),
            CellValue::new("1/1", SortKey::Int(100))
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "restarts"), &obj),
            CellValue::number(3)
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "age"), &obj).text.as_ref(),
            "2h"
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "image"), &obj),
            CellValue::text("registry.k8s.io/web:v1.3.2")
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "ip"), &obj),
            CellValue::text("10.244.3.17")
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "node"), &obj),
            CellValue::text("node-07.cluster.local")
        );
    }

    #[test]
    fn waiting_reason_wins_over_phase() {
        let obj = pod(
            "2026-09-22T00:00:00Z",
            "Running",
            false,
            9,
            Some("CrashLoopBackOff"),
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "status"), &obj),
            CellValue::text("CrashLoopBackOff")
        );
    }

    #[test]
    fn not_ready_container_shows_partial_ready_count() {
        let obj = pod("2026-09-22T00:00:00Z", "Pending", false, 0, None);
        assert_eq!(
            cell(&column_by_id("Pod", true, "ready"), &obj),
            CellValue::new("0/1", SortKey::Int(0))
        );
    }

    #[test]
    fn age_sort_key_uses_visible_age_direction() {
        let older = pod("2026-09-20T00:00:00Z", "Running", true, 0, None);
        let newer = pod("2026-09-22T00:00:00Z", "Running", true, 0, None);
        assert!(
            cell(&column_by_id("Pod", true, "age"), &older).key
                > cell(&column_by_id("Pod", true, "age"), &newer).key
        );
    }

    // The sort key must rank rows the same way the text reads.
    #[test]
    fn age_and_last_seen_sort_keys_follow_the_displayed_value() {
        let now = Timestamp::now().as_second();
        let future = Timestamp::from_second(now + 3_600).expect("valid timestamp");
        let skewed = pod(&future.to_string(), "Running", true, 0, None);
        let age = cell(&column_by_id("Pod", true, "age"), &skewed);
        assert_eq!(
            age.text.as_ref(),
            "0s",
            "a future clock must not show a minus"
        );
        assert_eq!(
            age.key,
            SortKey::Int(0),
            "the key matches the displayed age for a future timestamp"
        );

        let event = object(json!({
            "metadata": { "name": "web.abc", "uid": "u1" },
            "lastTimestamp": future.to_string(),
        }));
        let last_seen = cell(&column_by_id("Event", true, "last-seen"), &event);
        assert_eq!(last_seen.text.as_ref(), "0s");
        assert_eq!(last_seen.key, SortKey::Int(0));

        // Ascending Age puts the youngest row first, so the visible order
        // and the key order stay the same.
        let young = pod(
            &Timestamp::from_second(now - 30)
                .expect("valid timestamp")
                .to_string(),
            "Running",
            true,
            0,
            None,
        );
        let old = pod(
            &Timestamp::from_second(now - 7_200)
                .expect("valid timestamp")
                .to_string(),
            "Running",
            true,
            0,
            None,
        );
        let young_key = cell(&column_by_id("Pod", true, "age"), &young).key;
        let old_key = cell(&column_by_id("Pod", true, "age"), &old).key;
        assert!(young_key < old_key, "younger rows sort first");
        assert!(matches!(young_key, SortKey::Int(29..=30)));
        assert!(matches!(old_key, SortKey::Int(7_199..=7_200)));
    }

    #[test]
    fn format_age_boundaries() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3_599), "59m");
        assert_eq!(format_age(3_600), "1h");
        assert_eq!(format_age(86_399), "23h");
        assert_eq!(format_age(86_400), "1d");
    }

    #[test]
    fn missing_fields_yield_empty_cells() {
        let bare: DynamicObject = serde_json::from_value(json!({})).expect("empty object");
        for column in pod_columns() {
            let cell = column.column.cell(&bare);
            assert!(
                cell.text.is_empty() || cell.text.as_ref() == NOT_REPORTED,
                "Missing fields must render as empty cells or the not-reported dash: {}",
                column.column.id
            );
        }
    }

    // "The cluster has not answered" and "the answer is zero" are different
    // facts, and a blank cell cannot say which one it is. Every `Pending` pod
    // showed two blanks.
    #[test]
    fn an_unreported_pod_field_draws_a_dash_and_a_reported_zero_stays_zero() {
        let bare: DynamicObject = serde_json::from_value(json!({})).expect("empty object");
        assert_eq!(
            cell(&column_by_id("Pod", true, "ready"), &bare),
            CellValue::text(NOT_REPORTED)
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "restarts"), &bare),
            CellValue::text(NOT_REPORTED)
        );

        let reported = object(json!({
            "metadata": { "name": "web" },
            "status": { "phase": "Pending", "containerStatuses": [] },
        }));
        assert_eq!(
            cell(&column_by_id("Pod", true, "restarts"), &reported),
            CellValue::number(0),
            "an empty status array means zero restarts, not no answer"
        );
        assert_eq!(
            cell(&column_by_id("Pod", true, "ready"), &reported)
                .text
                .as_ref(),
            "0/0"
        );
    }

    // A Pod with a sidecar rendered one image with nothing to say a second
    // container existed.
    #[test]
    fn the_image_column_counts_the_containers_it_hides() {
        let single = object(json!({
            "metadata": { "name": "web" },
            "spec": { "containers": [{ "name": "app", "image": "app:1" }] },
        }));
        assert_eq!(
            cell(&column_by_id("Pod", true, "image"), &single),
            CellValue::text("app:1"),
            "one container needs no suffix"
        );

        let with_sidecar = object(json!({
            "metadata": { "name": "web" },
            "spec": {
                "containers": [
                    { "name": "app", "image": "app:1" },
                    { "name": "log-shipper", "image": "shipper:2" },
                    { "name": "proxy", "image": "proxy:3" },
                ],
            },
        }));
        assert_eq!(
            cell(&column_by_id("Pod", true, "image"), &with_sidecar),
            CellValue::text("app:1+2")
        );
    }

    #[test]
    fn specializations_match_kubectl_columns() {
        assert_eq!(
            titles("Deployment", true),
            [
                "Name",
                "Namespace",
                "Ready",
                "Up-to-Date",
                "Available",
                "Age"
            ]
        );
        assert_eq!(
            titles("Service", true),
            ["Name", "Namespace", "Type", "Cluster-IP", "Ports", "Age"]
        );
        assert_eq!(
            titles("Node", true),
            ["Name", "Status", "Roles", "Age", "Version"]
        );
        assert_eq!(
            titles("ConfigMap", true),
            ["Name", "Namespace", "Data", "Age"]
        );
        assert_eq!(titles("Secret", true), ["Name", "Namespace", "Data", "Age"]);
        assert_eq!(
            titles("Job", true),
            ["Name", "Namespace", "Completions", "Age"]
        );
        assert_eq!(
            titles("CronJob", true),
            ["Name", "Namespace", "Schedule", "Age"]
        );
        assert_eq!(
            titles("Event", true),
            ["Type", "Reason", "Object", "Last Seen"]
        );
    }

    #[test]
    fn namespace_column_follows_scope() {
        assert_eq!(
            titles("Deployment", false),
            ["Name", "Ready", "Up-to-Date", "Available", "Age"]
        );
        assert_eq!(
            titles("Service", false),
            ["Name", "Type", "Cluster-IP", "Ports", "Age"]
        );
        assert_eq!(titles("Widget", false), ["Name", "Age"]);
        assert_eq!(titles("Widget", true), ["Name", "Namespace", "Age"]);
        assert_eq!(
            titles("Node", false),
            ["Name", "Status", "Roles", "Age", "Version"]
        );
    }

    #[test]
    fn unknown_kind_falls_back_to_name_namespace_age() {
        let columns = columns_for("Widget", true);
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].column.id, "name");
        assert_eq!(columns[1].column.id, "namespace");
        assert_eq!(columns[2].column.id, "age");
    }

    #[test]
    fn deployment_cells_project_replica_counts() {
        let obj = object(json!({
            "metadata": { "name": "web", "namespace": "default", "uid": "u1" },
            "spec": { "replicas": 4 },
            "status": { "readyReplicas": 3, "updatedReplicas": 4, "availableReplicas": 2 },
        }));
        assert_eq!(
            cell(&column_by_id("Deployment", true, "ready"), &obj),
            CellValue::new("3/4", SortKey::Int(75))
        );
        assert_eq!(
            cell(&column_by_id("Deployment", true, "up-to-date"), &obj),
            CellValue::number(4)
        );
        assert_eq!(
            cell(&column_by_id("Deployment", true, "available"), &obj),
            CellValue::number(2)
        );

        let bare = object(json!({ "metadata": { "name": "web" } }));
        assert!(
            cell(&column_by_id("Deployment", true, "ready"), &bare)
                .text
                .is_empty()
        );
        assert!(
            cell(&column_by_id("Deployment", true, "available"), &bare)
                .text
                .is_empty()
        );
    }

    #[test]
    fn service_cells_project_type_ip_and_ports() {
        let obj = object(json!({
            "metadata": { "name": "web", "namespace": "default", "uid": "u1" },
            "spec": {
                "type": "NodePort",
                "clusterIP": "10.96.0.10",
                "ports": [
                    { "port": 80, "protocol": "TCP", "nodePort": 30080 },
                    { "port": 443 },
                ],
            },
        }));
        assert_eq!(
            cell(&column_by_id("Service", true, "type"), &obj),
            CellValue::text("NodePort")
        );
        assert_eq!(
            cell(&column_by_id("Service", true, "cluster-ip"), &obj),
            CellValue::text("10.96.0.10")
        );
        assert_eq!(
            cell(&column_by_id("Service", true, "ports"), &obj),
            CellValue::text("80:30080/TCP, 443/TCP")
        );

        let defaulted = object(json!({ "metadata": { "name": "web" }, "spec": {} }));
        assert_eq!(
            cell(&column_by_id("Service", true, "type"), &defaulted),
            CellValue::text("ClusterIP")
        );
    }

    #[test]
    fn node_cells_project_status_roles_and_version() {
        let obj = object(json!({
            "metadata": {
                "name": "node-1",
                "uid": "u1",
                "labels": {
                    "node-role.kubernetes.io/control-plane": "",
                    "kubernetes.io/os": "linux",
                },
            },
            "status": {
                "conditions": [{ "type": "Ready", "status": "True" }],
                "nodeInfo": { "kubeletVersion": "v1.31.0" },
            },
        }));
        assert_eq!(
            cell(&column_by_id("Node", false, "status"), &obj),
            CellValue::text("Ready")
        );
        assert_eq!(
            cell(&column_by_id("Node", false, "roles"), &obj),
            CellValue::text("control-plane")
        );
        assert_eq!(
            cell(&column_by_id("Node", false, "version"), &obj),
            CellValue::text("v1.31.0")
        );

        let worker = object(json!({
            "metadata": { "name": "node-2" },
            "status": { "conditions": [{ "type": "Ready", "status": "False" }] },
        }));
        assert_eq!(
            cell(&column_by_id("Node", false, "status"), &worker),
            CellValue::text("NotReady")
        );
        // A missing value is empty, like every other cell. The literal
        // `<none>` read as a tag rather than as a value in 12px monospace.
        assert!(
            cell(&column_by_id("Node", false, "roles"), &worker)
                .text
                .is_empty()
        );
    }

    #[test]
    fn data_and_job_cells_count_keys_and_completions() {
        let config_map = object(json!({
            "metadata": { "name": "cm" },
            "data": { "a": "1", "b": "2" },
            "binaryData": { "c": "AA==" },
        }));
        assert_eq!(
            cell(&column_by_id("ConfigMap", true, "data"), &config_map),
            CellValue::number(3)
        );

        let secret = object(json!({
            "metadata": { "name": "secret" },
            "data": { "token": "QQ==" },
        }));
        assert_eq!(
            cell(&column_by_id("Secret", true, "data"), &secret),
            CellValue::number(1)
        );

        let job = object(json!({
            "metadata": { "name": "migrate" },
            "spec": { "completions": 3 },
            "status": { "succeeded": 2 },
        }));
        assert_eq!(
            cell(&column_by_id("Job", true, "completions"), &job),
            CellValue::new("2/3", SortKey::Int(66))
        );

        let cron = object(json!({
            "metadata": { "name": "backup" },
            "spec": { "schedule": "*/5 * * * *" },
        }));
        assert_eq!(
            cell(&column_by_id("CronJob", true, "schedule"), &cron),
            CellValue::text("*/5 * * * *")
        );
    }

    #[test]
    fn event_cells_project_type_reason_object_and_age() {
        let now = Timestamp::now().as_second();
        let seen = Timestamp::from_second(now - 90).expect("valid timestamp");
        let obj = object(json!({
            "metadata": { "name": "web.abc", "namespace": "default", "uid": "u1" },
            "type": "Warning",
            "reason": "BackOff",
            "involvedObject": { "kind": "Pod", "name": "web-0", "namespace": "default" },
            "lastTimestamp": seen.to_string(),
        }));
        assert_eq!(
            cell(&column_by_id("Event", true, "type"), &obj),
            CellValue::text("Warning")
        );
        assert_eq!(
            cell(&column_by_id("Event", true, "reason"), &obj),
            CellValue::text("BackOff")
        );
        assert_eq!(
            cell(&column_by_id("Event", true, "object"), &obj),
            CellValue::text("Pod/web-0")
        );
        let last_seen = cell(&column_by_id("Event", true, "last-seen"), &obj);
        assert_eq!(last_seen.text.as_ref(), "1m");
        assert!(matches!(
            last_seen.key,
            SortKey::Int(seconds) if (89..=90).contains(&seconds)
        ));
    }
}
