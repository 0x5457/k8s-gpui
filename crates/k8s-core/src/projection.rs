//! Builds immutable filtered and sorted snapshots for read-only views.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use kube::ResourceExt;
use kube::core::DynamicObject;
use kube::core::{Selector, SelectorExt};

/// Cell text with a precomputed sort key.
#[derive(Clone, Debug, PartialEq)]
pub struct CellValue {
    pub text: Arc<str>,
    pub key: SortKey,
}

fn empty_text() -> Arc<str> {
    static EMPTY: OnceLock<Arc<str>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::from("")).clone()
}

impl CellValue {
    /// Use the same string for display and text sorting.
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        let text = text.into();
        Self {
            key: SortKey::Text(Arc::clone(&text)),
            text,
        }
    }

    /// Display decimal text and sort by number.
    pub fn number(value: i64) -> Self {
        Self {
            text: Arc::from(value.to_string()),
            key: SortKey::Int(value),
        }
    }

    /// Use different display text and sort key, such as an age value and timestamp.
    pub fn new(text: impl Into<Arc<str>>, key: SortKey) -> Self {
        Self {
            text: text.into(),
            key,
        }
    }

    /// Empty cell for a missing or inapplicable value.
    pub fn empty() -> Self {
        Self {
            text: empty_text(),
            key: SortKey::Null,
        }
    }
}

/// Sort key. `Null` sorts before numbers and text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SortKey {
    Null,
    Int(i64),
    Text(Arc<str>),
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (SortKey::Null, SortKey::Null) => Ordering::Equal,
            (SortKey::Null, _) => Ordering::Less,
            (_, SortKey::Null) => Ordering::Greater,
            (SortKey::Int(a), SortKey::Int(b)) => a.cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (SortKey::Int(_), SortKey::Text(_)) => Ordering::Less,
            (SortKey::Text(_), SortKey::Int(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type CellProjector = Arc<dyn Fn(&DynamicObject) -> CellValue + Send + Sync>;

/// Table column definition.
#[derive(Clone)]
pub struct Column {
    pub id: String,
    pub projector: CellProjector,
}

impl Column {
    pub fn new(
        id: impl Into<String>,
        projector: impl Fn(&DynamicObject) -> CellValue + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            projector: Arc::new(projector),
        }
    }

    pub fn cell(&self, obj: &DynamicObject) -> CellValue {
        (self.projector)(obj)
    }
}

/// Filter values. `None` disables that filter.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Exact namespace match. Cluster-scoped objects do not match.
    pub namespace: Option<String>,
    /// Case-insensitive Unicode name substring.
    pub name_substring: Option<String>,
    pub label_selector: Option<Selector>,
}

/// Snapshot row with precomputed cells.
#[derive(Clone, Debug)]
pub struct Row {
    pub obj: Arc<DynamicObject>,
    pub cells: Vec<CellValue>,
}

/// Immutable index snapshot.
#[derive(Clone, Debug, Default)]
pub struct IndexSnapshot {
    pub rows: Vec<Row>,
    /// UID to row index. Objects without a UID are omitted.
    pub by_uid: HashMap<String, usize>,
    pub generation: u64,
}

impl IndexSnapshot {
    pub fn row_by_uid(&self, uid: &str) -> Option<&Row> {
        self.by_uid.get(uid).and_then(|index| self.rows.get(*index))
    }
}

/// Sort column index and direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: usize,
    pub descending: bool,
}

impl Sort {
    pub fn ascending(column: usize) -> Self {
        Self {
            column,
            descending: false,
        }
    }

    pub fn descending(column: usize) -> Self {
        Self {
            column,
            descending: true,
        }
    }
}

/// Filter, project, and sort objects into an immutable snapshot.
pub fn build_snapshot<I, B>(
    items: I,
    columns: &[Column],
    filter: &Filter,
    sort: Option<&Sort>,
    generation: u64,
) -> IndexSnapshot
where
    I: IntoIterator<Item = B>,
    B: Into<Arc<DynamicObject>>,
{
    let name_needle = filter.name_substring.as_deref().map(str::to_lowercase);

    let mut rows: Vec<Row> = Vec::new();
    for item in items {
        let obj: Arc<DynamicObject> = item.into();
        if !matches_filter(&obj, filter, name_needle.as_deref()) {
            continue;
        }
        let cells = columns.iter().map(|column| column.cell(&obj)).collect();
        rows.push(Row { obj, cells });
    }

    if let Some(sort) = sort {
        rows.sort_by(|a, b| {
            let key_a = a.cells.get(sort.column).map(|cell| &cell.key);
            let key_b = b.cells.get(sort.column).map(|cell| &cell.key);
            key_a
                .cmp(&key_b)
                .then_with(|| tiebreak(&a.obj).cmp(&tiebreak(&b.obj)))
        });
        if sort.descending {
            rows.reverse();
        }
    }

    let by_uid = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.obj.metadata.uid.clone().map(|uid| (uid, index)))
        .collect();

    IndexSnapshot {
        rows,
        by_uid,
        generation,
    }
}

fn matches_filter(obj: &DynamicObject, filter: &Filter, name_needle: Option<&str>) -> bool {
    if let Some(namespace) = &filter.namespace
        && obj.metadata.namespace.as_deref() != Some(namespace.as_str())
    {
        return false;
    }
    if let Some(needle) = name_needle {
        let Some(name) = obj.metadata.name.as_deref() else {
            return false;
        };
        if !name.to_lowercase().contains(needle) {
            return false;
        }
    }
    if let Some(selector) = &filter.label_selector
        && !selector.matches(obj.labels())
    {
        return false;
    }
    true
}

fn tiebreak(obj: &DynamicObject) -> (Option<&str>, Option<&str>, &str) {
    (
        obj.metadata.uid.as_deref(),
        obj.metadata.namespace.as_deref(),
        obj.metadata.name.as_deref().unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::{ApiResource, Expression, GroupVersionKind};

    fn object(
        name: &str,
        namespace: Option<&str>,
        uid: Option<&str>,
        labels: &[(&str, &str)],
        replicas: Option<i64>,
    ) -> Arc<DynamicObject> {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "name".to_string(),
            serde_json::Value::String(name.to_string()),
        );
        if let Some(namespace) = namespace {
            metadata.insert(
                "namespace".to_string(),
                serde_json::Value::String(namespace.to_string()),
            );
        }
        if let Some(uid) = uid {
            metadata.insert(
                "uid".to_string(),
                serde_json::Value::String(uid.to_string()),
            );
        }
        if !labels.is_empty() {
            let labels: serde_json::Map<String, serde_json::Value> = labels
                .iter()
                .map(|(key, value)| {
                    (
                        (*key).to_string(),
                        serde_json::Value::String((*value).to_string()),
                    )
                })
                .collect();
            metadata.insert("labels".to_string(), serde_json::Value::Object(labels));
        }

        let mut value = serde_json::json!({ "metadata": metadata });
        if let Some(replicas) = replicas {
            value["spec"] = serde_json::json!({ "replicas": replicas });
        }
        Arc::new(serde_json::from_value(value).expect("synthetic DynamicObject"))
    }

    fn bare_object() -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(serde_json::json!({})).expect("synthetic empty DynamicObject"),
        )
    }

    fn name_column() -> Column {
        Column::new("name", |obj| match obj.metadata.name.as_deref() {
            Some(name) => CellValue::text(name),
            None => CellValue::empty(),
        })
    }

    fn replicas_column() -> Column {
        Column::new("replicas", |obj| {
            obj.data
                .get("spec")
                .and_then(|spec| spec.get("replicas"))
                .and_then(serde_json::Value::as_i64)
                .map_or_else(CellValue::empty, CellValue::number)
        })
    }

    fn names(snapshot: &IndexSnapshot) -> Vec<String> {
        snapshot
            .rows
            .iter()
            .map(|row| row.obj.metadata.name.clone().unwrap_or_default())
            .collect()
    }

    fn selector(expressions: impl IntoIterator<Item = Expression>) -> Selector {
        expressions.into_iter().collect()
    }

    #[test]
    fn text_cells_share_text_storage() {
        let cell = CellValue::text("shared text");
        let SortKey::Text(key) = &cell.key else {
            panic!("expected text sort key");
        };
        assert!(Arc::ptr_eq(&cell.text, key));
    }

    #[test]
    fn empty_input_yields_empty_snapshot() {
        let snapshot = build_snapshot(
            Vec::<Arc<DynamicObject>>::new(),
            &[name_column()],
            &Filter::default(),
            None,
            7,
        );
        assert!(snapshot.rows.is_empty());
        assert!(snapshot.by_uid.is_empty());
        assert_eq!(snapshot.generation, 7);
    }

    #[test]
    fn builds_cells_and_uid_index() {
        let objects = vec![
            object("alpha", Some("ns"), Some("uid-a"), &[], Some(3)),
            object("beta", None, Some("uid-b"), &[], None),
        ];
        let snapshot = build_snapshot(
            objects,
            &[name_column(), replicas_column()],
            &Filter::default(),
            None,
            1,
        );

        assert_eq!(names(&snapshot), ["alpha", "beta"]);
        assert_eq!(snapshot.rows[0].cells[0].text.as_ref(), "alpha");
        assert_eq!(snapshot.rows[0].cells[1], CellValue::number(3));
        assert_eq!(snapshot.rows[1].cells[1], CellValue::empty());
        assert_eq!(snapshot.by_uid.get("uid-a"), Some(&0));
        assert_eq!(snapshot.by_uid.get("uid-b"), Some(&1));
        assert_eq!(
            snapshot
                .row_by_uid("uid-b")
                .and_then(|row| row.obj.metadata.name.clone()),
            Some("beta".to_string())
        );
        assert!(snapshot.row_by_uid("missing").is_none());
    }

    #[test]
    fn namespace_filter_hits_and_rejects_cluster_scoped() {
        let objects = vec![
            object("a", Some("prod"), Some("u1"), &[], None),
            object("b", Some("dev"), Some("u2"), &[], None),
            object("c", None, Some("u3"), &[], None),
        ];

        let filtered = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                namespace: Some("prod".to_string()),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&filtered), ["a"]);

        let all = build_snapshot(objects, &[name_column()], &Filter::default(), None, 1);
        assert_eq!(names(&all).len(), 3);
    }

    #[test]
    fn name_substring_is_case_insensitive_and_unicode_aware() {
        let objects = vec![
            object("MyPod", Some("ns"), Some("u1"), &[], None),
            object("前端服务", Some("ns"), Some("u2"), &[], None),
            object("other", Some("ns"), Some("u3"), &[], None),
        ];

        let ascii = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                name_substring: Some("myp".to_string()),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&ascii), ["MyPod"]);

        let cjk = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                name_substring: Some("服务".to_string()),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&cjk), ["前端服务"]);

        let miss = build_snapshot(
            objects,
            &[name_column()],
            &Filter {
                name_substring: Some("absent".to_string()),
                ..Filter::default()
            },
            None,
            1,
        );
        assert!(miss.rows.is_empty());
    }

    #[test]
    fn label_selector_boundaries() {
        let web = object(
            "web",
            Some("ns"),
            Some("u1"),
            &[("app", "web"), ("tier", "frontend")],
            None,
        );
        let db = object("db", Some("ns"), Some("u2"), &[("app", "db")], None);
        let objects = vec![web, db];

        let equal = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                label_selector: Some(selector([Expression::Equal(
                    "app".to_string(),
                    "web".to_string(),
                )])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&equal), ["web"]);

        let in_set = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                label_selector: Some(selector([Expression::In(
                    "app".to_string(),
                    ["db".to_string()].into_iter().collect(),
                )])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&in_set), ["db"]);

        let not_in = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                label_selector: Some(selector([Expression::NotIn(
                    "app".to_string(),
                    ["web".to_string()].into_iter().collect(),
                )])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&not_in), ["db"]);

        let exists = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                label_selector: Some(selector([Expression::Exists("tier".to_string())])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&exists), ["web"]);

        let does_not_exist = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                label_selector: Some(selector([Expression::DoesNotExist("tier".to_string())])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&does_not_exist), ["db"]);

        let combined = build_snapshot(
            objects,
            &[name_column()],
            &Filter {
                label_selector: Some(selector([
                    Expression::Exists("app".to_string()),
                    Expression::NotEqual("app".to_string(), "web".to_string()),
                ])),
                ..Filter::default()
            },
            None,
            1,
        );
        assert_eq!(names(&combined), ["db"]);
    }

    #[test]
    fn sort_ascending_and_descending_by_key() {
        let objects = vec![
            object("c", None, Some("u3"), &[], Some(3)),
            object("a", None, Some("u1"), &[], Some(1)),
            object("b", None, Some("u2"), &[], Some(2)),
        ];

        let ascending = build_snapshot(
            objects.clone(),
            &[name_column(), replicas_column()],
            &Filter::default(),
            Some(&Sort::ascending(1)),
            1,
        );
        assert_eq!(names(&ascending), ["a", "b", "c"]);

        let descending = build_snapshot(
            objects,
            &[name_column(), replicas_column()],
            &Filter::default(),
            Some(&Sort::descending(1)),
            1,
        );
        assert_eq!(names(&descending), ["c", "b", "a"]);
    }

    #[test]
    fn sort_ties_break_by_uid_deterministically() {
        let mut objects = vec![
            object("c", None, Some("uid-3"), &[], Some(1)),
            object("a", None, Some("uid-1"), &[], Some(1)),
            object("b", None, Some("uid-2"), &[], Some(1)),
        ];
        let sort = Sort::ascending(0);
        let sorted = build_snapshot(
            objects.clone(),
            &[replicas_column()],
            &Filter::default(),
            Some(&sort),
            1,
        );
        assert_eq!(names(&sorted), ["a", "b", "c"]);

        objects.reverse();
        let sorted_again = build_snapshot(
            objects.clone(),
            &[replicas_column()],
            &Filter::default(),
            Some(&sort),
            1,
        );
        assert_eq!(names(&sorted_again), names(&sorted));

        // Descending order is the exact reverse of ascending order.
        let descending = build_snapshot(
            objects,
            &[replicas_column()],
            &Filter::default(),
            Some(&Sort::descending(0)),
            1,
        );
        assert_eq!(names(&descending), ["c", "b", "a"]);
    }

    #[test]
    fn null_sort_key_orders_before_values() {
        let objects = vec![
            object("has", None, Some("u1"), &[], Some(2)),
            object("missing", None, Some("u2"), &[], None),
        ];

        let ascending = build_snapshot(
            objects,
            &[replicas_column()],
            &Filter::default(),
            Some(&Sort::ascending(0)),
            1,
        );
        assert_eq!(names(&ascending), ["missing", "has"]);
    }

    #[test]
    fn number_cells_sort_numerically() {
        let objects = vec![
            object("nine", None, Some("u1"), &[], Some(9)),
            object("ten", None, Some("u2"), &[], Some(10)),
        ];

        let sorted = build_snapshot(
            objects,
            &[replicas_column()],
            &Filter::default(),
            Some(&Sort::ascending(0)),
            1,
        );
        assert_eq!(names(&sorted), ["nine", "ten"]);
        assert_eq!(sorted.rows[1].cells[0].text.as_ref(), "10");
    }

    #[test]
    fn out_of_range_sort_column_falls_back_to_tiebreak() {
        let objects = vec![
            object("b", None, Some("uid-2"), &[], None),
            object("a", None, Some("uid-1"), &[], None),
        ];

        let sorted = build_snapshot(
            objects,
            &[name_column()],
            &Filter::default(),
            Some(&Sort::ascending(9)),
            1,
        );
        assert_eq!(names(&sorted), ["a", "b"]);
    }

    #[test]
    fn objects_missing_name_uid_namespace_do_not_panic() {
        let objects = vec![bare_object(), object("named", None, None, &[], None)];

        let filtered = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter {
                namespace: Some("ns".to_string()),
                name_substring: Some("x".to_string()),
                label_selector: Some(selector([Expression::Exists("app".to_string())])),
            },
            Some(&Sort::ascending(0)),
            1,
        );
        assert!(filtered.rows.is_empty());

        let unfiltered = build_snapshot(
            objects,
            &[name_column()],
            &Filter::default(),
            Some(&Sort::ascending(0)),
            1,
        );
        assert_eq!(names(&unfiltered), ["", "named"]);
    }

    #[test]
    fn column_cell_uses_projector_on_dynamic_data() {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("apps", "v1", "Deployment"),
            "deployments",
        );
        let obj = DynamicObject::new("web", &resource)
            .data(serde_json::json!({ "spec": { "replicas": 4 } }));
        let column = replicas_column();
        assert_eq!(column.id, "replicas");
        assert_eq!(column.cell(&obj), CellValue::number(4));
    }
}
