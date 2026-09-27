//! Builds immutable filtered and sorted snapshots for read-only views.
//!
//! Three things live here because they have to be one definition rather than several:
//!
//! - **The query grammar** and the **column-header popover** (`Filter::set_values`). Both
//!   write the same query string, so a reader who types `status=Failed` and a reader who
//!   ticks `Failed` get the same table, and a filter can be copied out of the box and
//!   pasted anywhere. `UI-REDESIGN` §7 L5 B is explicit that two states would diverge.
//! - **Severity** ([`Severity`]), the grade the default view sorts on and the summary strip
//!   counts. `UI-REDESIGN` §7 L6 asks for it to be reusable by the table, the Overview and
//!   the sidebar, which means it cannot live in any of them.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::core::DynamicObject;
use kube::core::{Expression, Selector, SelectorExt};

/// How badly a row wants looking at, worst first.
///
/// `UI-REDESIGN` §7 L6 fixes the order this enum declares — `Failed` >
/// `CrashLoopBackOff` > pending past 5m > pending > running — so the declared order *is*
/// the default sort, read straight off the type rather than restated in a comparator that
/// could disagree with it.
///
/// `UI-SPEC` §0 铁律三 grades a pending row by how long it has been pending, because 9,900
/// pods that were scheduled seconds ago and 300 that have been waiting five minutes look
/// identical when they are the same colour. `Queued`, `Waiting` and `Stuck` are that rule:
/// one status word, three grades.
///
/// The variant order is also the tie-break a reader expects when two rows are equally bad —
/// a failed pod before a crash-looping one — so nothing here needs a second ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The cluster reported a failure: `Failed`, `Error`, `OOMKilled`, `Evicted`,
    /// `ImagePullBackOff`, `ErrImagePull`.
    Failed,
    /// A container that keeps being restarted: `CrashLoopBackOff`.
    CrashLooping,
    /// `Pending` past [`PENDING_STUCK`]. The scheduler has stopped.
    Stuck,
    /// `Pending` past [`PENDING_QUEUED`]: worth a second look.
    Waiting,
    /// `Pending` under [`PENDING_QUEUED`]: a queue that is moving.
    Queued,
    /// No verdict at all: `Terminating`, `Succeeded`, an empty status, or a word this app
    /// does not know. Grey, and sorted above healthy so it surfaces, because "the cluster
    /// has not told us" and "the cluster is fine" are different answers.
    Unknown,
    /// Running, and fine. Grey: `UI-SPEC` §0 铁律三 colours exceptions, not successes.
    Healthy,
}

/// A pending row under this is a queue, not a problem.
///
/// Thirty seconds is where a reader watching a rollout stops believing it is still working:
/// short enough to stay out of the way, long enough that an ordinary scheduling delay never
/// wears a colour.
pub const PENDING_QUEUED: Duration = Duration::from_secs(30);

/// A pending row past this is stuck.
///
/// Five minutes is past every normal scheduling path in Kubernetes, including a node that
/// is draining. Past it nothing is coming.
pub const PENDING_STUCK: Duration = Duration::from_secs(5 * 60);

impl Severity {
    /// The word a query and a facet list name this grade by.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Failed => "Failed",
            Severity::CrashLooping => "CrashLooping",
            Severity::Stuck => "Stuck",
            Severity::Waiting => "Waiting",
            Severity::Queued => "Queued",
            Severity::Unknown => "Unknown",
            Severity::Healthy => "Healthy",
        }
    }

    /// Every grade, worst first. The order a facet list and a popover draw them in.
    pub const ALL: [Severity; 7] = [
        Severity::Failed,
        Severity::CrashLooping,
        Severity::Stuck,
        Severity::Waiting,
        Severity::Queued,
        Severity::Unknown,
        Severity::Healthy,
    ];

    /// The grade of one object against a clock the caller has already read.
    ///
    /// The age ladder needs "now" and the clock is a syscall-ish read, so a build that
    /// grades ten thousand rows reads it once and hands it here. Every row of one build is
    /// then graded against the same instant, which is also what keeps the strip's count and
    /// the sort's order from disagreeing about a row that sits on the boundary.
    pub fn at(obj: &DynamicObject, now: i64) -> Severity {
        Self::from_status(status_text(obj), age_seconds_at(obj, now))
    }

    /// The grade of a status word and an age, for a caller that has both already.
    ///
    /// A missing age grades the quiet way: a cluster that never sent a creation timestamp
    /// has said nothing about how long anything has been waiting, and colouring a row on the
    /// strength of a field that is not there is the same lie as reading `Unknown` as healthy.
    pub fn from_status(status: Option<&str>, age: Option<Duration>) -> Severity {
        let queued = matches!(
            status,
            Some("Pending" | "ContainerCreating" | "PodInitializing")
        );
        if queued {
            return match age {
                Some(age) if age > PENDING_STUCK => Severity::Stuck,
                Some(age) if age > PENDING_QUEUED => Severity::Waiting,
                _ => Severity::Queued,
            };
        }
        match status {
            Some("CrashLoopBackOff") => Severity::CrashLooping,
            Some(
                "Failed"
                | "Error"
                | "OOMKilled"
                | "Evicted"
                | "ImagePullBackOff"
                | "ErrImagePull"
                | "CreateContainerConfigError"
                | "CreateContainerError"
                | "RunContainerError",
            ) => Severity::Failed,
            Some("Running" | "Succeeded" | "Active" | "Ready" | "Bound") => Severity::Healthy,
            // `Terminating` is here rather than in the waiting grades: a pod being deleted
            // is a state the cluster is in, not something wrong with it, and grading it
            // amber painted every rollout and every `kubectl delete` as a problem.
            _ => Severity::Unknown,
        }
    }
}

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

/// A row is listed when every predicate in the filter holds for it.
/// One build's clock, so the whole file can be read against one rule.
///
/// Every predicate needs "now" to grade an age, and reading it per row is a clock
/// read per row: a table of ten thousand pods filtered by `age>5m` took ten thousand
/// of them for the same answer. The instant is therefore read once by [`project`] and
/// handed down here, which is also what keeps a row that sits on the 30s boundary out
/// of one half of the table and into the other.
fn matches_filter(obj: &DynamicObject, filter: &Filter, now: i64) -> bool {
    filter.preds.iter().all(|pred| pred.matches_at(obj, now))
}

/// The labels of one object, without the copy [`kube::ResourceExt::labels`] makes.
///
/// A selector over ten thousand rows was cloning every label of every row into a fresh map
/// to answer a question the object already holds the answer to. `None` is an object with no
/// labels, which no `Selector` clause can match: an existence test is false and an equality
/// test is false, so there is nothing to match against and the answer is always false.
fn labels_match(selector: &Selector, obj: &DynamicObject) -> bool {
    obj.metadata
        .labels
        .as_ref()
        .is_some_and(|labels| selector.matches(labels))
}

/// Whether `haystack` contains `needle`, ignoring case, without allocating.
///
/// This is on the path of every row of every snapshot for the two clauses a table is almost
/// always filtered by, and it used to lowercase both sides: two allocations per row per
/// clause, sixty thousand of them for a ten-thousand-row table filtered by one word.
///
/// The ASCII path compares windows, which is the same answer `to_lowercase` gives for ASCII
/// and costs nothing. Anything else falls back to lowercasing, because Unicode case folding
/// is not something a byte comparison can answer.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        return haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()));
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Whether `haystack` equals any of the values, ignoring case.
fn any_equals_ignore_case(haystack: &str, values: &[Arc<str>]) -> bool {
    values
        .iter()
        .any(|value| haystack.eq_ignore_ascii_case(value))
}

/// Whether `haystack` holds any of the values as a case-insensitive substring, which is
/// what `~` asks for.
fn any_contains_ignore_case(haystack: &str, values: &[Arc<str>]) -> bool {
    values
        .iter()
        .any(|value| contains_ignore_case(haystack, value))
}

/// The name text a substring or exact clause compares against.
fn name_matches(obj: &DynamicObject, text: &str, exact: bool) -> bool {
    let Some(name) = obj.metadata.name.as_deref() else {
        return false;
    };
    if exact {
        return name == text;
    }
    contains_ignore_case(name, text)
}

/// A JSON pointer read, for the fields that live in the object body rather than
/// in its metadata.
fn text_at<'a>(data: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    data.pointer(pointer)?.as_str()
}

/// The status the columns show: a container's waiting reason, or the pod phase.
///
/// The walk is spelled out rather than written as two JSON pointers because a pointer
/// re-reads and re-parses its path on every call, and this runs once per row per grading —
/// ten thousand times for a table that is merely open.
fn status_text(obj: &DynamicObject) -> Option<&str> {
    obj.data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
        .and_then(|statuses| {
            statuses.iter().find_map(|container| {
                container
                    .get("state")
                    .and_then(|state| state.get("waiting"))
                    .and_then(|waiting| waiting.get("reason"))
                    .and_then(serde_json::Value::as_str)
            })
        })
        .or_else(|| text_at(&obj.data, "/status/phase"))
}

/// The owner reference an `owner=` clause names.
///
/// The **controller** when the object has one, and the first owner reference when it
/// does not. This used to be the first reference unconditionally, which is the same
/// answer for a Pod (one owner, a ReplicaSet) and a different one for an object with
/// two — and a different answer is the whole problem, because `UI-REDESIGN` §7 L3
/// promises that following a Related row lands on the object the controller will
/// recreate, while `WRITE-OPS.md` §2.2 judges the same object's fate from
/// [`crate::ops::controller_of`]. One predicate, not two judgements: the flag read
/// here is the flag that function reads, and a test holds the two answers together
/// (see `a_query_names_the_owner_the_confirmation_names`).
///
/// Borrowed rather than obtained from [`crate::ops::controller_of`], which owns its
/// three strings: this runs on every row of every filtered snapshot, and an owned
/// `Controller` would be three allocations per row to learn one fact.
fn owner_reference(obj: &DynamicObject) -> Option<&OwnerReference> {
    let owners = obj.metadata.owner_references.as_deref()?;
    owners
        .iter()
        .find(|owner| owner.controller.unwrap_or(false))
        .or_else(|| owners.first())
}

/// The UID of the owner an `owner=` clause names.
fn owner_uid(obj: &DynamicObject) -> Option<&str> {
    Some(owner_reference(obj)?.uid.as_str())
}

/// The type a field compares in, which is a property of the field rather than of
/// any object: a reader writes `age>5m` and `age` is a duration whatever it holds.
fn field_kind(field: Field) -> Kind {
    match field {
        Field::Restarts | Field::Ready => Kind::Number,
        Field::Age => Kind::Duration,
        _ => Kind::Text,
    }
}

/// What a field yields, as the grammar compares it. A property of the field, so the
/// parser can tell `age>5m` from `restarts>3` without a second lookup table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Number,
    Duration,
}

/// The reported container statuses, or `None` when the cluster has not sent them.
fn containers(obj: &DynamicObject) -> Option<&Vec<serde_json::Value>> {
    obj.data.get("status")?.get("containerStatuses")?.as_array()
}

/// Sums one number over `containerStatuses`, or `None` when the cluster has not
/// sent the list.
fn container_sum(obj: &DynamicObject, field: &str) -> Option<i64> {
    let statuses = containers(obj)?;
    Some(
        statuses
            .iter()
            .filter_map(|container| {
                container
                    .get(field.trim_start_matches('/'))
                    .and_then(serde_json::Value::as_i64)
            })
            .sum(),
    )
}

/// Seconds since the object was created, which is the value the `Age` column
/// sorts on and therefore the value `age>` compares.
///
/// The clock is read once per build and handed down rather than read per row: it is one
/// syscall-shaped read ten thousand times, and reading it per row buys nothing, because a
/// filter does not stop ageing between two rows of one build. It is read again for the next
/// build, which is what keeps `age>5m` getting truer.
fn age_seconds_at(obj: &DynamicObject, now: i64) -> Option<Duration> {
    let created = obj.metadata.creation_timestamp.as_ref()?.0.as_second();
    Some(Duration::from_secs(
        u64::try_from(now.saturating_sub(created).max(0)).unwrap_or(0),
    ))
}

/// The wall clock, as whole seconds.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

impl Pred {
    /// Whether this predicate holds for one object.
    pub fn matches(&self, obj: &DynamicObject) -> bool {
        self.matches_at(obj, now_seconds())
    }

    /// Whether this predicate holds for one object, against a clock the caller has read.
    pub fn matches_at(&self, obj: &DynamicObject, now: i64) -> bool {
        match self {
            Pred::Name { text, exact } => name_matches(obj, text, *exact),
            // A forced full-text search reads the name, the namespace and the
            // labels, because "full text" is the whole of the text a resource
            // carries that a reader can see.
            Pred::FullText { text } => {
                name_matches(obj, text, false)
                    || obj
                        .metadata
                        .namespace
                        .as_deref()
                        .is_some_and(|value| contains_ignore_case(value, text))
                    || obj.metadata.labels.iter().flatten().any(|(key, value)| {
                        contains_ignore_case(key, text) || contains_ignore_case(value, text)
                    })
            }
            Pred::Field {
                field,
                compare,
                value,
                negated,
            } => {
                let holds = field_compares(*field, *compare, value, obj, now);
                holds != *negated
            }
            Pred::Labels {
                selector, negated, ..
            } => labels_match(selector, obj) != *negated,
            Pred::Not(inner) => !inner.matches_at(obj, now),
        }
    }
}

/// Compares one field against one right-hand side.
fn field_compares(
    field: Field,
    compare: Compare,
    right: &Right,
    obj: &DynamicObject,
    now: i64,
) -> bool {
    let left = field.value_at(obj, now);
    match (&left, right) {
        (Value::Missing, _) => {
            // Nothing to compare. `!=` is the one comparison that holds, because
            // an object with no status is not `Running`; every other comparison is
            // a claim about a value nobody reported, and answering it either way
            // would be inventing one.
            compare == Compare::NotEqual
        }
        (Value::Text(left), Right::Text(right)) => {
            // A set clause asks "is this value one of these", and a missing value
            // from the set is the same answer as a value that is not in it — which
            // is what makes `status=Failed` a filter rather than a test of whether
            // the reader spelled the status the way the cluster does.
            match compare {
                Compare::Equal => any_equals_ignore_case(left, right),
                Compare::NotEqual => !any_equals_ignore_case(left, right),
                // `~` against a set is a substring of any of them, so a reader who
                // wrote `status~Run` gets the one status and not "nothing, because
                // no value equals `Run`".
                Compare::Contains => any_contains_ignore_case(left, right),
                Compare::Greater => right.iter().any(|value| *left > &**value),
                Compare::GreaterOrEqual => right.iter().any(|value| *left >= &**value),
                Compare::Less => right.iter().any(|value| *left < &**value),
                Compare::LessOrEqual => right.iter().any(|value| *left <= &**value),
            }
        }
        (Value::Number(left), Right::Number(right)) => match compare {
            Compare::Equal => left == right,
            Compare::NotEqual => left != right,
            Compare::Contains => left.to_string().contains(&right.to_string()),
            Compare::Greater => left > right,
            Compare::GreaterOrEqual => left >= right,
            Compare::Less => left < right,
            Compare::LessOrEqual => left <= right,
        },
        (Value::Duration(left), Right::Duration(right)) => match compare {
            Compare::Equal => left == right,
            Compare::NotEqual => left != right,
            // A duration has no substring. `age~5m` is nonsense rather than an
            // error, because the query box is a live filter and refusing to filter
            // while a reader types is worse than ignoring the clause.
            Compare::Contains => false,
            Compare::Greater => left > right,
            Compare::GreaterOrEqual => left >= right,
            Compare::Less => left < right,
            Compare::LessOrEqual => left <= right,
        },
        // A clause whose right-hand side is the wrong type for its field cannot
        // hold, except under `!=`.
        _ => compare == Compare::NotEqual,
    }
}

/// A field a query clause can compare against, named the way the columns are.
///
/// `age` is a duration and everything else is text or a number, so the grammar
/// can tell `age>5m` from `restarts>3` without a second lookup table: a field
/// that yields a number is compared numerically, and one that yields a duration
/// parses its right-hand side as a duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Namespace,
    Name,
    Status,
    Restarts,
    Age,
    Node,
    Ready,
    Owner,
    /// The grade [`Severity`] gives the row, which no column id spells.
    ///
    /// This is the field that makes `Needs attention` and the popover's `Only problems`
    /// row writable as a query string. Both used to be a status clause plus a severity
    /// predicate the interface held privately, so the string a reader copied out of the box
    /// did not reproduce the table they were looking at — two states for one filter, which
    /// is the failure `UI-REDESIGN` §7 L5 B exists to prevent.
    ///
    /// Its values are grades rather than cluster words, which is the distinction from
    /// `status`: `status=CrashLoopBackOff` names what the kubelet said,
    /// `severity=CrashLooping` names how much the app thinks that is worth.
    Severity,
}

impl Field {
    /// The field a clause names, or `None` when the word is not a field.
    ///
    /// `ns` is the short spelling the grammar documents; every other name is the column id,
    /// so a clause and a header cannot drift apart. Field names are read without regard to
    /// case, because the values are and a reader who writes `Status=` has not written a
    /// different filter from one who writes `status=` — only a different spelling of the
    /// same question, and answering it with an error teaches nothing.
    pub fn parse(name: &str) -> Option<Field> {
        if name.eq_ignore_ascii_case("ns") || name.eq_ignore_ascii_case("namespace") {
            return Some(Field::Namespace);
        }
        if name.eq_ignore_ascii_case("name") {
            return Some(Field::Name);
        }
        if name.eq_ignore_ascii_case("status") {
            return Some(Field::Status);
        }
        if name.eq_ignore_ascii_case("restarts") {
            return Some(Field::Restarts);
        }
        if name.eq_ignore_ascii_case("age") {
            return Some(Field::Age);
        }
        if name.eq_ignore_ascii_case("node") {
            return Some(Field::Node);
        }
        if name.eq_ignore_ascii_case("ready") {
            return Some(Field::Ready);
        }
        if name.eq_ignore_ascii_case("owner") {
            return Some(Field::Owner);
        }
        if name.eq_ignore_ascii_case("severity") {
            return Some(Field::Severity);
        }
        None
    }

    /// The field's canonical name in a query.
    ///
    /// The same word [`Field::parse`] accepts, so rendering a clause produces
    /// something that parses back to the clause it came from. A namespace is `ns`
    /// because that is the short spelling the grammar documents, and a rendered
    /// `namespace=` would be a second spelling to keep in step.
    pub fn as_str(self) -> &'static str {
        match self {
            Field::Namespace => "ns",
            Field::Name => "name",
            Field::Status => "status",
            Field::Restarts => "restarts",
            Field::Age => "age",
            Field::Node => "node",
            Field::Ready => "ready",
            Field::Owner => "owner",
            Field::Severity => "severity",
        }
    }

    /// The value this field has on one object, in the type the grammar compares.
    pub fn value(self, obj: &DynamicObject) -> Value<'_> {
        self.value_at(obj, now_seconds())
    }

    /// [`Field::value`] against a clock the caller has already read.
    ///
    /// Text fields borrow out of the object. They used to be copied into a fresh `Arc<str>`
    /// per row per clause, which is two heap allocations for every row a table filters, and
    /// a filter over ten thousand rows is two hundred thousand of them to learn that a row's
    /// namespace is the namespace it had a moment ago.
    pub fn value_at<'a>(self, obj: &'a DynamicObject, now: i64) -> Value<'a> {
        let text = |value: Option<&'a str>| match value {
            Some(value) => Value::Text(value),
            None => Value::Missing,
        };
        match self {
            Field::Namespace => text(obj.metadata.namespace.as_deref()),
            Field::Name => text(obj.metadata.name.as_deref()),
            Field::Node => text(text_at(&obj.data, "/spec/nodeName")),
            Field::Status => text(status_text(obj)),
            Field::Owner => text(owner_uid(obj)),
            Field::Severity => text(Some(Severity::at(obj, now).as_str())),
            // A container list the cluster has not sent yet is a missing value
            // rather than a zero: `restarts>0` on a pod whose containers are not
            // reported is not a claim that it has never restarted.
            Field::Restarts => match container_sum(obj, "/restartCount") {
                Some(total) => Value::Number(total as f64),
                None => Value::Missing,
            },
            Field::Ready => match containers(obj) {
                Some(statuses) => Value::Number(
                    statuses
                        .iter()
                        .filter(|container| {
                            container
                                .get("ready")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false)
                        })
                        .count() as f64,
                ),
                None => Value::Missing,
            },
            Field::Age => match age_seconds_at(obj, now) {
                Some(age) => Value::Duration(age),
                None => Value::Missing,
            },
        }
    }
}

/// What a field yields for one object.
///
/// The text borrows from the object rather than owning it: a filter reads every row of
/// every snapshot and an owned string is an allocation per row per clause for a value that
/// was already sitting in the object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<'a> {
    Text(&'a str),
    Number(f64),
    Duration(Duration),
    /// The cluster did not report the field. A comparison against it is false
    /// except for `!=`, which is true — an object that has no status is not
    /// `Running`, and dropping it would hide the rows with the least information.
    Missing,
}

/// How a clause compares a field against its right-hand side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    Equal,
    NotEqual,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    /// A substring, case-insensitive. `status~Run`.
    Contains,
}

impl Compare {
    fn parse(symbol: &str) -> Option<Compare> {
        match symbol {
            "=" => Some(Compare::Equal),
            "!=" => Some(Compare::NotEqual),
            ">" => Some(Compare::Greater),
            ">=" => Some(Compare::GreaterOrEqual),
            "<" => Some(Compare::Less),
            "<=" => Some(Compare::LessOrEqual),
            "~" => Some(Compare::Contains),
            _ => None,
        }
    }

    /// The comparison as it is written in a query, so a clause can be rendered
    /// back into the string it came from.
    pub fn symbol(self) -> &'static str {
        match self {
            Compare::Equal => "=",
            Compare::NotEqual => "!=",
            Compare::Greater => ">",
            Compare::GreaterOrEqual => ">=",
            Compare::Less => "<",
            Compare::LessOrEqual => "<=",
            Compare::Contains => "~",
        }
    }
}

/// One clause of a query: what has to be true of an object for it to be listed.
#[derive(Clone, Debug)]
pub enum Pred {
    /// A bare word or a quoted one: a substring of the name, or the whole name
    /// when the reader quoted it.
    Name { text: Arc<str>, exact: bool },
    /// A forced full-text search, which is `q` in the query. It is the same
    /// substring match as a bare word, kept as its own variant so a caller can
    /// tell "the reader asked for this text" from "the reader typed a word that
    /// happened to be a field name" — the reason `q` exists.
    FullText { text: Arc<str> },
    /// `status!=Running`, `restarts>3`, `age>5m`, and the rest of the fields.
    Field {
        field: Field,
        compare: Compare,
        value: Right,
        negated: bool,
    },
    /// `label:app=api`, which reuses the same [`Selector`] the API takes.
    ///
    /// The clause is kept as its source text as well as the selector, because a
    /// `Selector` cannot be read back out — it holds its expressions privately —
    /// and rendering the query back has to reproduce what the reader typed rather
    /// than a re-derivation of it that would drift.
    Labels {
        selector: Selector,
        text: Arc<str>,
        negated: bool,
    },
    /// `!anything`. Negation is a wrapper rather than a flag on every variant so
    /// that a clause can be turned over without the grammar teaching each
    /// comparison what "not" means.
    Not(Box<Pred>),
}

/// The right-hand side of a comparison, in the type the field compares in.
#[derive(Clone, Debug)]
pub enum Right {
    /// One or more values, which is how a clause takes a set: `ns=prod` is a set of
    /// one and `ns=prod,staging` is a set of two.
    ///
    /// A set rather than an optional second variant because a set of one has to
    /// behave like a single value everywhere — the popover writes `status=Failed`
    /// for one ticked box and `status=Running,Failed` for two, and both are the
    /// same clause with a different number of members. That is also what makes the
    /// popover a *general* mechanism: it never needs a second grammar for "one".
    Text(Vec<Arc<str>>),
    Number(f64),
    Duration(Duration),
}

impl Right {
    /// The right-hand side the way the reader wrote it.
    pub fn text(&self) -> String {
        match self {
            Right::Text(values) => values
                .iter()
                .map(|value| &**value)
                .collect::<Vec<_>>()
                .join(","),
            Right::Number(value) => value.to_string(),
            Right::Duration(value) => format_duration(*value),
        }
    }

    /// The values as separate strings, which is what a set-valued clause is read
    /// as by the popover and by a caller toggling one of them.
    pub fn values(&self) -> Vec<String> {
        match self {
            Right::Text(values) => values.iter().map(|value| value.to_string()).collect(),
            Right::Number(value) => vec![value.to_string()],
            Right::Duration(value) => vec![format_duration(*value)],
        }
    }
}

/// A located parse failure: which token, where it was, and what was expected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// The token that could not be read, in the source query.
    pub token: String,
    /// Its byte range in the source, so the query box can point at it.
    pub span: crate::fuzzy::Span,
    /// What the reader should have written instead.
    pub expected: String,
}

impl QueryError {
    /// The error as one line, which is what the query box shows under itself.
    pub fn message(&self) -> String {
        format!("`{}` — {}", self.token, self.expected)
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for QueryError {}

/// The parsed query. Every predicate has to hold for a row to be listed.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub preds: Vec<Pred>,
}

impl Filter {
    /// A filter that excludes nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Parses a query string into a filter.
    pub fn parse(query: &str) -> Result<Filter, QueryError> {
        Ok(Filter {
            preds: parse_query(query)?,
        })
    }

    /// Renders the predicates back into a query string.
    ///
    /// This is what makes the query box and the column-header popover one thing:
    /// the popover reads the predicates, changes one, writes the string back, and
    /// the box shows the result. Without it the two would each keep their own
    /// state and diverge, which is the failure the design names.
    pub fn to_query(&self) -> String {
        self.preds
            .iter()
            .map(pred_to_query)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether this filter excludes nothing.
    pub fn is_empty(&self) -> bool {
        self.preds.is_empty()
    }

    /// The needle a relevance ranking should order by, which is the text the
    /// reader typed rather than the clauses they wrote.
    pub fn search_needle(&self) -> Option<&str> {
        self.preds.iter().find_map(|pred| match pred {
            Pred::Name { text, exact: false } => Some(text.as_ref()),
            Pred::FullText { text } => Some(text.as_ref()),
            _ => None,
        })
    }

    /// Whether this filter is exactly the clauses of `query`.
    ///
    /// The round trip the two entry points share: a popover that changed the
    /// filter has to produce a string that parses back to it.
    pub fn matches_query(&self, query: &str) -> bool {
        Filter::parse(query)
            .map(|parsed| parsed.preds == self.preds)
            .unwrap_or(false)
    }
}

/// A query string with `field`'s value set replaced by `values`, and an empty list
/// clearing the clause.
///
/// This is the normalization every entry point goes through, and it is a *string* edit on
/// purpose. Re-rendering the whole query from the parsed clauses would rewrite the reader's
/// words in the parser's spelling — `age>90s` would become `age>1m30s`, `status=running`
/// would become `status=Running` — under the pointer, while they were typing. So the
/// source text of every other clause is copied out of the query with its own quotes intact
/// and only the clause being changed is rewritten.
///
/// Two queries that name the same values are one filter and render as one filter, so the
/// values are sorted: `ns=prod,staging` and `ns=staging,prod` are the same string, and a
/// saved view does not care which order the reader ticked them in.
pub fn set_field_values(query: &str, field: Field, values: &[String]) -> String {
    let Ok(tokens) = crate::fuzzy::tokenize(query) else {
        // A query that does not parse has no clauses to rewrite, so it is left whole
        // rather than half-rebuilt; the reader is already being told what is wrong with it.
        return query.trim().to_owned();
    };
    let mut kept: Vec<&str> = Vec::new();
    for token in tokens {
        // The source text, not the decoded word: a quoted name with a space in it is one
        // clause and has to stay one clause. `Span` sits on character boundaries.
        let clause = query[token.span.start..token.span.end].trim();
        if !clause.is_empty() && !clause_is_on(clause, field) {
            kept.push(clause);
        }
    }
    let mut sorted: Vec<&str> = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::with_capacity(query.len() + 16);
    for clause in kept {
        out.push_str(clause);
        out.push(' ');
    }
    if !sorted.is_empty() {
        out.push_str(field.as_str());
        out.push('=');
        out.push_str(&values_to_query(&sorted));
    }
    out.trim_end().to_owned()
}

/// The clause a string spells, or `None` when it is not one clause about `field`.
fn clause_is_on(clause: &str, field: Field) -> bool {
    parse_query(clause).is_ok_and(|preds| {
        preds.len() == 1 && preds.iter().any(|pred| is_field_clause(pred, field))
    })
}

/// Whether a predicate is a clause on `field`, whatever its values.
///
/// Read off the parsed clause rather than by string prefix, so `status!=Running` is
/// recognised and a token that merely starts with the same letters is not.
pub fn is_field_clause(pred: &Pred, field: Field) -> bool {
    matches!(pred, Pred::Field { field: this, .. } if *this == field)
}

/// The values a field's clause allows, or `None` when the query says nothing about it.
///
/// `None` and `Some(vec![])` are different answers and both are reachable: a reader who
/// unticks every value has asked for no rows, and a reader who has not touched the column
/// has asked for all of them. Conflating them would make the last untick jump from nothing
/// to everything.
pub fn allowed_values(preds: &[Pred], field: Field) -> Option<Vec<String>> {
    let mut values = preds.iter().find_map(|pred| match pred {
        Pred::Field {
            field: this,
            compare: Compare::Equal,
            value: Right::Text(values),
            negated: false,
        } if *this == field => Some(values.clone()),
        _ => None,
    })?;
    // Sorted, because the popover compares membership and the query's own order is the
    // reader's, not a set's.
    values.sort_unstable();
    Some(values.iter().map(|value| value.to_string()).collect())
}

/// The values of a set, as the query writes them: comma-separated, and a value that would
/// not survive being written bare is quoted.
fn values_to_query<S: AsRef<str>>(values: &[S]) -> String {
    values
        .iter()
        .map(|value| {
            let value = value.as_ref();
            if value.contains(',') || value.contains(char::is_whitespace) {
                crate::fuzzy::quoted_word(value)
            } else {
                value.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl PartialEq for Pred {
    fn eq(&self, other: &Self) -> bool {
        pred_to_query(self) == pred_to_query(other)
    }
}

impl PartialEq for Right {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Right::Number(left), Right::Number(right)) => left == right,
            (Right::Duration(left), Right::Duration(right)) => left == right,
            _ => self.text() == other.text(),
        }
    }
}

/// Parses a query string into its clauses.
///
/// The grammar is one clause per token, and every clause is independent: a row is
/// listed when every clause holds. That is what makes the column-header popover
/// and the query box the same thing — adding a clause is adding a predicate, and
/// the popover only ever adds or removes predicates.
///
/// ```text
/// ns=prod                  namespace, exact; several with ns=a,b
/// status!=Running          any known field
/// restarts>3               numeric comparison
/// age>5m                   duration
/// label:app=api tier=web   label selector, reusing the existing Selector
/// "exact name"             quotes mean exact
/// !node                    negation
/// q text                   forced full text
/// ```
pub fn parse_query(query: &str) -> Result<Vec<Pred>, QueryError> {
    let tokens = crate::fuzzy::tokenize(query).map_err(|error| QueryError {
        token: error.span.slice(query).to_owned(),
        span: error.span,
        expected: error.message,
    })?;

    let mut preds = Vec::with_capacity(tokens.len());
    // `q` forces every following clause to be a text search, so a field name and
    // a search for the word are the same thing to the reader and different things
    // to the parser.
    let mut force_text = false;
    for token in tokens {
        if token.text == "q" {
            force_text = true;
            continue;
        }
        let clause = parse_clause(query, &token, force_text)?;
        force_text = false;
        match clause {
            None => continue,
            Some(pred) => preds.push(pred),
        }
    }
    Ok(union_equal_fields(preds))
}

/// Merges two equality clauses on the same field into one value set.
///
/// `ns=prod ns=staging` is two ways of writing one question. Read as a conjunction it is a
/// filter nothing can satisfy, and a reader who typed it gets an empty table with no
/// explanation — the one outcome the grammar never means. A row has one namespace, one
/// status, one node, so the intersection of two sets on one of those fields is empty, and
/// the only reading left is the union the set form already spells `ns=prod,staging`.
///
/// Only `=` clauses merge, and only ones that are not negated: `status!=Running` is a claim
/// about everything *else* rather than a second list, and a number has nothing to union.
fn union_equal_fields(preds: Vec<Pred>) -> Vec<Pred> {
    let mut merged: Vec<Pred> = Vec::with_capacity(preds.len());
    for pred in preds {
        let Pred::Field {
            field,
            compare: Compare::Equal,
            value: Right::Text(values),
            negated: false,
        } = &pred
        else {
            merged.push(pred);
            continue;
        };
        // The first clause on this field is where its values live; a later one is folded
        // into it and dropped, so the order the reader wrote the fields in is the order
        // they are rendered in.
        let Some(first) = merged.iter_mut().find_map(|existing| match existing {
            Pred::Field {
                field: this,
                compare: Compare::Equal,
                value: Right::Text(held),
                negated: false,
            } if *this == *field => Some(held),
            _ => None,
        }) else {
            merged.push(pred);
            continue;
        };
        for value in values {
            if !first.iter().any(|held| held.eq_ignore_ascii_case(value)) {
                first.push(Arc::clone(value));
            }
        }
    }
    merged
}

/// One clause of a query, or `None` when the token was only a `q` marker.
fn parse_clause(
    query: &str,
    token: &crate::fuzzy::Token,
    force_text: bool,
) -> Result<Option<Pred>, QueryError> {
    let (text, negated) = match token.text.strip_prefix('!') {
        Some(rest) if !rest.is_empty() => (rest, true),
        _ => (token.text.as_str(), false),
    };

    // A token that is nothing but operators is a clause the reader has not finished
    // writing. It used to become a name search for the operator characters, which matches
    // no pod name and so silently emptied the table: the reader who typed `status=` on the
    // way to `status=Failed` saw every row disappear and no hairline to say why.
    if !token.quoted && !force_text && is_operator_only(text) {
        return Err(QueryError {
            token: token.text.clone(),
            span: token.span,
            expected: format!(
                "a field name before `{text}`. Fields: {}",
                KNOWN_FIELDS.join(", ")
            ),
        });
    }

    // A quoted token is a name, and a quoted token with no operator is that name
    // in full. `label:app="my app"` quotes only the value, so the operator is
    // looked for outside the quotes rather than in the decoded text.
    let clause = if token.quoted {
        Pred::Name {
            text: Arc::from(text),
            exact: true,
        }
    } else if force_text || text.is_empty() {
        Pred::FullText {
            text: Arc::from(text),
        }
    } else if let Some(clause) = parse_label(query, token)? {
        clause
    } else {
        match split_comparison(token) {
            None => Pred::Name {
                text: Arc::from(text),
                exact: false,
            },
            Some(clause) => parse_comparison(clause, token)?,
        }
    };
    // `!` in front of anything turns the whole clause over, including a name
    // search: `!node` is "no name contains node", which is the reading that makes
    // the prefix worth having.
    Ok(Some(if negated {
        Pred::Not(Box::new(clause))
    } else {
        clause
    }))
}

/// Whether a word is nothing but the characters the grammar reads as operators.
fn is_operator_only(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| matches!(ch, '=' | '!' | '>' | '<' | '~'))
}

/// A `field<op>value` clause, in whatever type the field compares in.
fn parse_comparison(
    (name, compare, value): (&str, Compare, &str),
    token: &crate::fuzzy::Token,
) -> Result<Pred, QueryError> {
    let error = |expected: &str| QueryError {
        token: token.text.clone(),
        span: token.span,
        expected: expected.to_owned(),
    };
    let field = Field::parse(name).ok_or_else(|| {
        error(&format!(
            "unknown field `{name}`. Known fields: {}",
            KNOWN_FIELDS.join(", ")
        ))
    })?;
    let right = match field_kind(field) {
        // The type comes from the field and the value is read against it, so a
        // duration and a number cannot be swapped by accident. A numeric field
        // takes one value: `restarts>1,2` is not a comparison with anything.
        Kind::Duration => Right::Duration(
            parse_duration(value)
                .ok_or_else(|| error("expected a duration such as `30s`, `5m`, `2h` or `3d`"))?,
        ),
        Kind::Number => {
            if value.contains(',') {
                return Err(error("expected one number; a range is not a comparison"));
            }
            Right::Number(
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| error("expected a number"))?,
            )
        }
        // A text field takes a set, because `ns=a,b` is the documented way to name
        // several and because a set of one is what a popover writes for one ticked
        // box. One code path, so the two can never disagree.
        _ => {
            let values = value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(Arc::<str>::from)
                .collect::<Vec<_>>();
            if values.is_empty() {
                return Err(error(&format!(
                    "expected a value after `{name}{}`",
                    compare.symbol()
                )));
            }
            Right::Text(values)
        }
    };
    Ok(Pred::Field {
        field,
        compare,
        value: right,
        negated: false,
    })
}

/// Every field name the grammar accepts, in the order the error lists them.
///
/// One list, so the "unknown field" error cannot offer a set of names the parser
/// does not in fact take — which is the failure mode of a hand-written message and
/// the reason it is derived instead.
const KNOWN_FIELDS: &[&str] = &[
    "ns", "name", "status", "restarts", "age", "node", "ready", "owner", "severity",
];

/// The `label:` clauses, which are the existing `Selector` rather than a second
/// syntax for it.
fn parse_label(query: &str, token: &crate::fuzzy::Token) -> Result<Option<Pred>, QueryError> {
    let error = |expected: &str| QueryError {
        token: token.text.clone(),
        span: token.span,
        expected: expected.to_owned(),
    };
    let Some(body) = token.text.strip_prefix("label:") else {
        return Ok(None);
    };
    if body.is_empty() {
        return Err(error(
            "expected a label after `label:`, such as `label:app=api`",
        ));
    }
    // A leading `!` inside the body is the selector's own negation, so
    // `label:!app` is "has no app label" and is not the clause's negation.
    let (body, inner) = match body.strip_prefix('!') {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let expression = match body.split_once('=') {
        Some((_, "")) => {
            return Err(error("expected a value after `=`"));
        }
        Some((key, value)) => {
            let values = value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            match values.as_slice() {
                [one] => Expression::Equal((*key).to_owned(), one.clone()),
                many => Expression::In((*key).to_owned(), many.iter().cloned().collect()),
            }
        }
        None if body.is_empty() => return Err(error("expected a label after `label:!`")),
        None if inner => Expression::DoesNotExist(body.to_owned()),
        None => Expression::Exists(body.to_owned()),
    };
    Ok(Some(Pred::Labels {
        selector: [expression].into_iter().collect(),
        // The source of the clause, quotes and all, so rendering the query back
        // reproduces what the reader typed rather than a normalised form of it.
        // The span includes the clause's own `!`, which the caller writes again.
        text: Arc::from(token.span.slice(query).trim_start_matches('!')),
        negated: false,
    }))
}

/// Splits `field<op>value` at the first operator that is not part of a longer
/// one, so `status!=Running` reads as `!=` and not as `!` then `=`.
fn split_comparison(token: &crate::fuzzy::Token) -> Option<(&str, Compare, &str)> {
    let text = token.text.as_str();
    for (index, _) in text.char_indices() {
        let op = ["!=", ">=", "<=", "=", ">", "<", "~"]
            .into_iter()
            .find(|op| text[index..].starts_with(op));
        let Some(op) = op else {
            continue;
        };
        // A `!` before the operator is the clause's negation, not the field's
        // name, so the field is what follows the last one.
        let head = &text[..index];
        let field = match head.rfind('!') {
            Some(bang) => &head[bang + 1..],
            None => head,
        };
        if field.is_empty() {
            continue;
        }
        return Some((field, Compare::parse(op)?, text[index + op.len()..].trim()));
    }
    None
}

/// Parses a duration literal: a number and a unit, several in a row, so `1h30m`
/// is the ninety minutes it reads as.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // A bare number is seconds, because every duration in a Kubernetes manifest
    // that is written as a number is a count of seconds.
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let mut total = Duration::ZERO;
    let mut rest = text;
    let mut read_any = false;
    while !rest.is_empty() {
        let digits = rest
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let value: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|ch: char| ch.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let part = match unit {
            "s" | "sec" | "secs" => Duration::from_secs(value),
            "m" | "min" | "mins" => Duration::from_secs(value.checked_mul(60)?),
            "h" | "hr" | "hrs" => Duration::from_secs(value.checked_mul(3_600)?),
            "d" | "day" | "days" => Duration::from_secs(value.checked_mul(86_400)?),
            // A unit-less number after the first pair has no length, so the rest
            // is not a duration.
            "" => return None,
            _ => return None,
        };
        total = total.checked_add(part)?;
        read_any = true;
    }
    read_any.then_some(total)
}

/// A duration as the query box writes it: the largest unit that is exact.
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        return "0s".to_owned();
    }
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

/// One predicate, rendered back into the query it came from.
fn pred_to_query(pred: &Pred) -> String {
    match pred {
        Pred::Name { text, exact: true } => crate::fuzzy::quoted_word(text),
        Pred::Name { text, .. } | Pred::FullText { text } => {
            // Quoted whenever a plain word could not carry it: whitespace splits the
            // query, and a quote or a trailing backslash ends the run or escapes out
            // of it. A name that contains one of those and was written unquoted is
            // still rendered quoted, because the reader is owed a filter that reads
            // back as the filter they asked for.
            if text.contains(char::is_whitespace) || text.contains('"') || text.contains('\\') {
                crate::fuzzy::quoted_word(text)
            } else {
                text.to_string()
            }
        }
        Pred::Field {
            field,
            compare,
            value,
            negated,
        } => format!(
            "{}{}{}{}",
            if *negated { "!" } else { "" },
            field.as_str(),
            compare.symbol(),
            // `values_to_query`, not `value.text()`. `text()` joins the set with commas
            // and hands back the raw values, so a clause the reader wrote as
            // `name="my pod"` came back as `name=my pod` — which no longer parses as one
            // clause. It failed silently: the next parse read `name=my` and a full-text
            // `pod`, so the filter quietly matched a different set of rows, and a saved
            // view persisted the corrupted string. `UI-REDESIGN` §7 L5 B makes "what it
            // writes is what the box then holds" the invariant, and this was the one path
            // that broke it. `values_to_query` quotes exactly the values that need it,
            // which is also what `set_field_values` writes, so the two agree.
            match value {
                Right::Text(_) => values_to_query(&value.values()),
                _ => value.text(),
            }
        ),
        Pred::Labels { text, negated, .. } => {
            format!("{}{}", if *negated { "!" } else { "" }, text)
        }
        Pred::Not(inner) => format!("!{}", pred_to_query(inner)),
    }
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

/// The columns a table is sorted by, in precedence order, and what a click does to them.
///
/// `UI-REDESIGN` §7 L5 C: one column by default, `Shift` with a click to add the next one
/// ("sort by status, then by age"). Precedence is the order the columns are in here, and
/// [`SortPlan::direction`] is how a header reads its own arrow back out, so the header and
/// the row order cannot be produced by two different pieces of state.
///
/// The default view is [`SortPlan::severity`] rather than a column: `UI-REDESIGN` §7 L6
/// fixes the first sort of a workload table as worst-first, which is a grade and not a
/// column value. Carrying it as a leading key rather than pretending it is a column is what
/// keeps a header from having to claim a column owns an order it does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SortPlan {
    columns: Vec<Sort>,
    severity_first: bool,
}

impl SortPlan {
    /// No sort at all, which is the order the rows arrive in.
    pub fn none() -> Self {
        Self::default()
    }

    /// One column, ascending.
    pub fn of(sort: Sort) -> Self {
        Self {
            columns: vec![sort],
            severity_first: false,
        }
    }

    /// The `Needs attention` order of `UI-REDESIGN` §7 L6: worst first, then by name.
    ///
    /// The name is the tie-break rather than a second clickable column because a table
    /// whose rows are equally bad should still read in a stable, predictable order.
    pub fn severity() -> Self {
        Self {
            columns: vec![],
            severity_first: true,
        }
    }

    /// One column out of the `Option<Sort>` a table used to hold, so a caller can move to a
    /// plan without rewriting its state.
    pub fn from_sort(sort: Option<Sort>) -> Self {
        sort.map_or_else(Self::none, Self::of)
    }

    /// The columns, most significant first.
    pub fn columns(&self) -> &[Sort] {
        &self.columns
    }

    /// Whether the rows are ordered worst-first before any column.
    pub fn is_severity_first(&self) -> bool {
        self.severity_first
    }

    /// Whether this plan orders the rows at all.
    pub fn is_sorted(&self) -> bool {
        self.severity_first || !self.columns.is_empty()
    }

    /// Whether `column` is in this plan, and which way it runs.
    ///
    /// This is what a header draws. `Some(false)` is ascending and `Some(true)` descending;
    /// `None` is a column the reader has not chosen, which is a different thing from a
    /// column sorted in either direction.
    pub fn direction(&self, column: usize) -> Option<bool> {
        self.columns
            .iter()
            .find(|sort| sort.column == column)
            .map(|sort| sort.descending)
    }

    /// Which column in the precedence order `column` is, counted from one.
    ///
    /// The `1` `2` a multi-column header draws next to its arrow, and it comes from the
    /// same list [`SortPlan::direction`] reads so the number beside an arrow and the
    /// position that decides the order cannot be produced by two pieces of state. `None`
    /// is a column the reader has not chosen, which draws no number at all.
    pub fn precedence(&self, column: usize) -> Option<usize> {
        self.columns
            .iter()
            .position(|sort| sort.column == column)
            .map(|index| index + 1)
    }

    /// What a click on a header does.
    ///
    /// - A plain click **replaces** the plan with that one column, ascending; a second
    ///   plain click on the same column reverses it. A plain click is a reader saying
    ///   which column they want, and `UI-REDESIGN` §7 L5 C fixes the default at one
    ///   column — a plain click that quietly grew the plan into a multi-column one would
    ///   make a two-column sort the state nobody asked for.
    /// - `Shift` **adds** a column at the end, or reverses one already in the plan, so
    ///   "sort by status, then by age" is two clicks with the second one held. This is
    ///   the only way to reach a second column, which is what keeps the plan readable
    ///   in a header that has to draw a number beside each arrow.
    ///
    /// A severity-first plan survives a click as the tie-break rather than disappearing:
    /// the reader chose the worst-first order by opening the table, and choosing a column
    /// to sort inside it is not the same request as choosing to stop.
    pub fn cycle(&mut self, column: usize, additive: bool) {
        if !additive {
            // A second plain click on the column that is already the whole plan reverses
            // it. Clearing and pushing `ascending` unconditionally meant the header could
            // never sort descending: the reader clicked `Age` and got oldest-last, clicked
            // again and got the same thing, and the only route to `descending` was a
            // `Shift`-click, which *adds* a second key. `UI-SPEC` §9.3 and
            // `PROMPT.md` §2.4 both ask for native list behaviour, and in a native list
            // the header is a two-state control.
            //
            // Two states, not three: ascending and descending. A column is taken out of
            // the plan by clicking a different one, which is the same rule the additive
            // arm below already follows.
            let reverses_sole_column = self.columns.len() == 1 && self.columns[0].column == column;
            self.columns.clear();
            self.columns.push(Sort {
                column,
                descending: reverses_sole_column,
            });
            // A plain click is the reader taking over the ordering, so the default
            // severity lead stops. `UI-REDESIGN` §7 L6 says severity applies "只对第一次
            // 打开这个 kind 生效；用户手动排序后不再覆盖" — and nothing else cleared this flag,
            // so on a table that opened severity-first every manual sort was silently
            // subordinate to it: clicking `Age` left the rows in exactly the order they were
            // already in, which reads as a dead header. `Shift` keeps the grade, because
            // adding a key is not taking over.
            self.severity_first = false;
            return;
        }
        match self.columns.iter().position(|sort| sort.column == column) {
            // Reversing rather than removing: a reader who adds a column and then adds
            // it again meant "and the other way", not "and then nothing". A column is
            // taken out of the plan by clicking a different one without `Shift`.
            Some(existing) => {
                self.columns[existing].descending = !self.columns[existing].descending
            }
            None => self.columns.push(Sort::ascending(column)),
        }
    }
}

/// Filter, project, and sort objects into an immutable snapshot.
///
/// One column, and no severity key: the shape a table has always had. [`build_snapshot_ordered`]
/// is the same build with the full [`SortPlan`], and this delegates to it so there is one
/// implementation.
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
    build_snapshot_ordered(
        items,
        columns,
        filter,
        &SortPlan::from_sort(sort.copied()),
        generation,
    )
}

/// Filter, project, and sort objects into an immutable snapshot, by a [`SortPlan`].
///
/// The snapshot is the unit of "what the table shows": it is built once, read by every
/// surface, and replaced rather than edited. Nothing here reads the clock per row — one
/// instant is read per build and every row of that build is graded and aged against it.
pub fn build_snapshot_ordered<I, B>(
    items: I,
    columns: &[Column],
    filter: &Filter,
    sort: &SortPlan,
    generation: u64,
) -> IndexSnapshot
where
    I: IntoIterator<Item = B>,
    B: Into<Arc<DynamicObject>>,
{
    let objects = items.into_iter().map(B::into);
    let built = project(objects, columns, filter, sort, None.as_ref());
    IndexSnapshot {
        rows: built.rows,
        by_uid: built.by_uid,
        generation,
    }
}

/// The last snapshot, kept so a rebuild can reuse the cells of rows that did not change.
///
/// `UI-REDESIGN` §7 L1's first lever: a rebuild that recomputed ten thousand rows of cells
/// for ten watch events was doing nine-tenths of its work to produce the same bytes. A row
/// is reused when the object behind it is the same object the cluster last sent — same UID,
/// same `resourceVersion` — and rebuilt otherwise.
///
/// The columns are remembered with it, because a reused row's cells were produced by those
/// columns' projectors. A caller that changes the columns of a live table gets correct
/// cells, not stale ones: a different set of column ids drops the cache rather than reusing
/// a row whose cells mean something else.
#[derive(Debug, Default)]
pub struct SnapshotCache {
    previous: Option<(Arc<IndexSnapshot>, Vec<String>)>,
}

impl SnapshotCache {
    /// Whether there is anything to reuse yet.
    pub fn is_empty(&self) -> bool {
        self.previous.is_none()
    }

    /// Rebuilds the snapshot, reusing the cells of every row whose object has not changed.
    pub fn rebuild<I, B>(
        &mut self,
        items: I,
        columns: &[Column],
        filter: &Filter,
        sort: &SortPlan,
        generation: u64,
    ) -> Arc<IndexSnapshot>
    where
        I: IntoIterator<Item = B>,
        B: Into<Arc<DynamicObject>>,
    {
        let ids: Vec<String> = columns.iter().map(|column| column.id.clone()).collect();
        // Reuse is dropped, not repaired, when the columns are not the ones the cached rows
        // were built with. Rebuilding is cheap enough that guessing would be worse.
        let reuse = match &self.previous {
            Some((snapshot, previous)) if *previous == ids => Some(Arc::clone(snapshot)),
            _ => None,
        };
        let objects = items.into_iter().map(B::into);
        let built = project(objects, columns, filter, sort, reuse.as_deref());
        let snapshot = Arc::new(IndexSnapshot {
            rows: built.rows,
            by_uid: built.by_uid,
            generation,
        });
        self.previous = Some((Arc::clone(&snapshot), ids));
        snapshot
    }
}

/// A built snapshot before it is handed out.
struct Built {
    rows: Vec<Row>,
    by_uid: HashMap<String, usize>,
}

/// The smallest row vector a build allocates when the caller gave no size hint.
///
/// A table of two rows should not reserve for a table of ten thousand, and a caller whose
/// hint is zero is a caller streaming. 256 is where the old flat reserve sat, so the
/// fallback costs exactly what it used to and the hinted path costs nothing extra.
const MIN_ROW_CAPACITY: usize = 256;

/// Whether an object is the same one the last snapshot was built from.
///
/// The UID says it is the same *resource* and the `resourceVersion` says the server has not
/// sent a newer version of it. Together they are the cluster's own answer to "did this
/// change", which is a stronger statement than comparing anything the app derived: a pod
/// whose age cell has gone stale keeps its cells, and a pod whose restart count moved does
/// not, with no field list to keep in step.
fn is_unchanged(previous: &IndexSnapshot, obj: &DynamicObject) -> Option<usize> {
    let uid = obj.metadata.uid.as_deref()?;
    let index = *previous.by_uid.get(uid)?;
    let row = previous.rows.get(index)?;
    (row.obj.metadata.resource_version.as_deref() == obj.metadata.resource_version.as_deref())
        .then_some(index)
}

/// Projects, filters and orders objects into the parts of a snapshot.
///
/// One function behind both the cold build and the cached rebuild, so the two cannot drift:
/// what counts as "changed" is decided here, next to the filter that decides which rows
/// exist at all.
fn project(
    objects: impl IntoIterator<Item = Arc<DynamicObject>>,
    columns: &[Column],
    filter: &Filter,
    sort: &SortPlan,
    previous: Option<&IndexSnapshot>,
) -> Built {
    let now = now_seconds();
    let objects = objects.into_iter();
    // Sized from what the caller said it is handing over, not from a guess.
    //
    // The store's `objects()` is a `Vec`, so its size hint is the exact row count and the
    // table is allocated once. It used to start at a flat 256 and grow, which on a
    // ten-thousand-row table is six reallocations, each one copying every row already
    // built — `UI-REDESIGN` §7 L1's third named cause, measured as fourteen
    // reallocations before the hint was used. A caller with no hint (a filtered stream)
    // still gets the floor, because a small table should not reserve for a large one.
    let mut rows: Vec<Row> = Vec::with_capacity(objects.size_hint().0.max(MIN_ROW_CAPACITY));
    // The position in the previous snapshot where the next row is looked for first.
    //
    // A rebuild almost always gets the same rows in the same order — the store hands out
    // its objects in the order it holds them — so the row at this position is the row that
    // was built last time, and finding it is two string comparisons instead of a hash of a
    // UID. It is verified like any other reuse (same UID, same `resourceVersion`), so a
    // reorder or a changed filter falls back to the map rather than reusing the wrong row.
    let mut hint = 0usize;
    for obj in objects {
        if !matches_filter(&obj, filter, now) {
            continue;
        }
        let cells = previous.and_then(|previous| {
            let at_hint = previous.rows.get(hint).filter(|row| {
                row.obj.metadata.uid.as_deref() == obj.metadata.uid.as_deref()
                    && row.obj.metadata.resource_version.as_deref()
                        == obj.metadata.resource_version.as_deref()
                    && row.cells.len() == columns.len()
            });
            if at_hint.is_some() {
                hint += 1;
            }
            at_hint
                .or_else(|| {
                    is_unchanged(previous, &obj)
                        .and_then(|index| previous.rows.get(index))
                        .filter(|row| row.cells.len() == columns.len())
                })
                .map(|row| row.cells.clone())
        });
        let cells =
            cells.unwrap_or_else(|| columns.iter().map(|column| column.cell(&obj)).collect());
        rows.push(Row { obj, cells });
    }

    order_rows(&mut rows, sort, now);

    let mut by_uid = HashMap::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        if let Some(uid) = &row.obj.metadata.uid {
            by_uid.insert(uid.clone(), index);
        }
    }

    Built { rows, by_uid }
}

/// Orders rows by a plan.
///
/// The keys are taken out of the rows before the sort and the rows are permuted afterwards,
/// which is two flat passes instead of a comparator that chases a pointer into every row on
/// every comparison: a ten-thousand-row name sort is a hundred thousand comparisons, and each
/// one used to load a `Vec` that lived somewhere else.
///
/// The direction is folded into each comparison rather than reversing the result at the end.
/// Reversing after an ascending sort also reverses the tie-break, so two rows that compared
/// equal came out in the opposite order depending on which way the column was pointing, and
/// a descending sort of a name column listed `zebra` before `aardvark` on equal names.
fn order_rows(rows: &mut [Row], plan: &SortPlan, now: i64) {
    if !plan.is_sorted() || rows.len() < 2 {
        return;
    }
    let columns = plan.columns();
    let grades: Option<Vec<u8>> = plan.severity_first.then(|| {
        rows.iter()
            .map(|row| Severity::at(&row.obj, now) as u8)
            .collect()
    });
    let mut keys: Vec<SortKey> = Vec::with_capacity(rows.len() * columns.len());
    for row in rows.iter() {
        for sort in columns {
            keys.push(sort_key(row, sort.column).clone());
        }
    }
    let width = columns.len().max(1);
    // The tie-break is materialized too. It is consulted for every pair of rows that
    // compares equal on the columns, which in a table with nine thousand pending pods is
    // most pairs, and reading it out of a flat array is one cache line rather than a walk
    // through two `Arc`s into a metadata block.
    let ties: Vec<(Option<&str>, Option<&str>, &str)> =
        rows.iter().map(|row| tiebreak(&row.obj)).collect();
    let mut order: Vec<u32> = (0..u32::try_from(rows.len()).unwrap_or(u32::MAX)).collect();
    order.sort_by(|&left, &right| {
        if let Some(grades) = &grades
            && let (Some(left), Some(right)) =
                (grades.get(left as usize), grades.get(right as usize))
        {
            // Worst first: a higher grade is a worse row, and the default view leads with
            // the rows that need looking at.
            let by_grade = right.cmp(left);
            if by_grade != Ordering::Equal {
                return by_grade;
            }
        }
        for (offset, sort) in columns.iter().enumerate() {
            let by_column = keys[(left as usize) * width + offset]
                .cmp(&keys[(right as usize) * width + offset]);
            if by_column != Ordering::Equal {
                return if sort.descending {
                    by_column.reverse()
                } else {
                    by_column
                };
            }
        }
        // Always ascending, whatever the column directions are: a tie is a tie, and a
        // tie-break that flipped with the arrow is a row that moves when the reader
        // reverses a column they were only looking at.
        ties[left as usize].cmp(&ties[right as usize])
    });
    permute(rows, &mut order);
}

/// The sort key of one column of one row. A column the row does not have sorts as null,
/// which is the same answer as an empty cell.
fn sort_key(row: &Row, column: usize) -> &SortKey {
    static NULL: OnceLock<SortKey> = OnceLock::new();
    row.cells
        .get(column)
        .map_or_else(|| NULL.get_or_init(|| SortKey::Null), |cell| &cell.key)
}

/// Rewrites `rows` so that the row at position `i` is the one `order[i]` names.
///
/// The in-place cycle walk, because a sort of indices has to be applied to something: doing
/// it with `swap` rather than by cloning every row means reusing a row's cells costs one
/// pointer move instead of a fresh `Vec` per row.
///
/// `order` is a sort of `0..len()`, so it reads as *the source of each slot*:
/// `order[slot]` is the row that ends up at `slot`. The cycle walk moves a row to its
/// **destination**, which is the other direction, so `order` is inverted first. Walking
/// `order` directly is a permutation whose answer is the inverse of the one asked for — and
/// on a three-row table the two differ by a rotation, which is invisible on two rows (a swap
/// is its own inverse) and wrong on every odd cycle above that: a name sort of `pod-0`,
/// `pod-1`, `pod-2` came back as `pod-1`, `pod-2`, `pod-0`, and `by_uid` was rebuilt from
/// that, so every index-keyed lookup in the app pointed at the wrong row.
fn permute(rows: &mut [Row], order: &mut [u32]) {
    let mut destination: Vec<u32> = vec![0; order.len()];
    for (slot, &source) in order.iter().enumerate() {
        destination[source as usize] = slot as u32;
    }
    for index in 0..destination.len() {
        while destination[index] != index as u32 {
            let target = destination[index] as usize;
            rows.swap(index, target);
            destination.swap(index, target);
        }
    }
}

fn tiebreak(obj: &DynamicObject) -> (Option<&str>, Option<&str>, &str) {
    (
        obj.metadata.uid.as_deref(),
        obj.metadata.namespace.as_deref(),
        obj.metadata.name.as_deref().unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// C8 · The Apply preview's diff (`WRITE-OPS.md` §3.1)
//
// It is here and not in the Inspector because it is the same reading of an object the
// table's columns are: one file that knows a Pod is `spec.containers[name=api].image`,
// rather than a projection for the grid and a second one for the preview that can disagree
// about what a field is called and which of them matter. A diff that disagreed with the
// table about a name would produce a preview nobody can match against what they edited.
// ---------------------------------------------------------------------------

/// One row of the Related block: what it is, and the query that opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Related {
    /// `Deployment`, `Node`, `Service` — the kind as the cluster spells it.
    pub kind: String,
    pub name: String,
    /// The query that opens a list of this relationship's rows.
    ///
    /// A string, not a [`Filter`] the caller builds, for the reason `UI-REDESIGN` §7 L5 B
    /// gives: a filter the reader can copy out of the box is a filter that reproduces the
    /// table they clicked, and a `Filter` assembled here would be one more state beside
    /// the query.
    pub query: String,
    /// Whether following this row lands on the object that will recreate the selected one.
    ///
    /// Read from [`crate::ops::controller_of`], the same call the write-side confirmation
    /// quotes, so the Related block and "It will be recreated automatically" cannot
    /// describe two different objects.
    pub is_controller: bool,
}

/// What deleting or editing an object will cost, in the terms `WRITE-OPS.md` §2.2 uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recreation {
    /// A controller owns it, so the cluster will make another.
    Managed { kind: String, name: String },
    /// Nothing owns it, so it will not come back.
    Unmanaged { owners: usize },
}

impl Recreation {
    /// Whether the object comes back by itself. The one question §2.2 turns the
    /// confirmation copy on.
    pub fn is_managed(self) -> bool {
        matches!(self, Recreation::Managed { .. })
    }
}

/// The relationships of one object, in the order the Related block lists them.
///
/// The controller first, then the other owners, then the node: the row a reader is most
/// likely to follow leads, and the node is the one relationship every Pod has and nothing
/// else does.
///
/// The controller question is asked once, by [`crate::ops::controller_of`], and both this
/// function and [`Recreation`] read that answer. It used to be derived separately here and
/// there, which is how a Pod with two owner references ended up with a Related block
/// pointing at one Deployment and a confirmation promising another.
pub fn relations(obj: &DynamicObject) -> Vec<Related> {
    let controller = crate::ops::controller_of(obj);
    let mut rows = Vec::new();
    for owner in obj.metadata.owner_references.iter().flatten() {
        if owner.name.is_empty() {
            continue;
        }
        let is_controller = controller
            .as_ref()
            .is_some_and(|found| found.uid == owner.uid);
        rows.push(Related {
            kind: owner.kind.clone(),
            name: owner.name.clone(),
            query: set_field_values("", Field::Owner, std::slice::from_ref(&owner.uid)),
            is_controller,
        });
    }
    if let Some(node) = text_at(&obj.data, "/spec/nodeName") {
        rows.push(Related {
            kind: "Node".to_owned(),
            name: node.to_owned(),
            query: set_field_values("", Field::Node, &[node.to_owned()]),
            is_controller: false,
        });
    }
    rows
}

/// What happens to an object if the reader changes or deletes it.
///
/// `WRITE-OPS.md` §2.2: one fact decides the whole confirmation — does something own
/// this. An owner reference without `controller: true` is lineage a reader may want to
/// see and is **not** a promise that the object will come back, so it counts as an owner
/// here and not as a controller.
pub fn recreation(obj: &DynamicObject) -> Recreation {
    match crate::ops::controller_of(obj) {
        Some(controller) => Recreation::Managed {
            kind: controller.kind,
            name: controller.name,
        },
        None => Recreation::Unmanaged {
            owners: obj
                .metadata
                .owner_references
                .as_deref()
                .map_or(0, |owners| owners.len()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::{ApiResource, GroupVersionKind};

    /// The in-place permutation has to put the row `order[i]` names at slot `i`.
    ///
    /// It used to apply the *inverse* permutation instead, which is invisible on two rows
    /// (a swap is its own inverse) and wrong on every odd cycle above that. A name sort of
    /// `pod-0`, `pod-1`, `pod-2` came back rotated, `by_uid` was rebuilt from the rotated
    /// rows, and every index-keyed lookup in the app pointed at the wrong row — which is
    /// what three table tests were reporting before the walk was fixed. Enumerated over
    /// every permutation of every length up to six, because the failure is a function of
    /// the cycle structure and a three-row case alone would not have caught a regression
    /// that broke the transpositions.
    #[test]
    fn permute_puts_the_row_order_names_at_its_slot() {
        // A row's name carries its original position, so a permutation that lands
        // rows on the wrong slots is visible as a sequence that is not the one asked
        // for. `0..=6` is every permutation of six elements, 873 of them.
        fn check(order: &[u32]) {
            let identity: Vec<String> = (0..order.len())
                .map(|index| format!("row-{index}"))
                .collect();
            let wanted: Vec<String> = order
                .iter()
                .map(|&source| identity[source as usize].clone())
                .collect();
            let mut rows: Vec<Row> = identity
                .iter()
                .map(|name| Row {
                    obj: object(name, None, None, &[], None),
                    cells: Vec::new(),
                })
                .collect();
            let mut order = order.to_vec();
            // The walk consumes `order` — it is an in-place cycle walk, and the
            // permutation it is walking is finished when it returns — so the input
            // is kept for the failure message. A message that printed the walked
            // vector would name a permutation nobody asked for.
            let asked = order.clone();
            permute(&mut rows, &mut order);
            let got: Vec<String> = rows
                .iter()
                .map(|row| row.obj.metadata.name.clone().unwrap_or_default())
                .collect();
            assert_eq!(got, wanted, "order {asked:?}");
        }

        fn permutations(prefix: &mut Vec<u32>, rest: &mut Vec<u32>, out: &mut Vec<Vec<u32>>) {
            if rest.is_empty() {
                out.push(prefix.clone());
                return;
            }
            for index in 0..rest.len() {
                let value = rest.remove(index);
                prefix.push(value);
                permutations(prefix, rest, out);
                prefix.pop();
                rest.insert(index, value);
            }
        }

        for length in 0..=6usize {
            let mut rest: Vec<u32> = (0..length as u32).collect();
            let mut orders = Vec::new();
            permutations(&mut Vec::new(), &mut rest, &mut orders);
            for order in &orders {
                check(order);
            }
        }
    }

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

    /// The multi-column rules of `UI-REDESIGN` §7 L5 C, as one table.
    ///
    /// They are not obvious and they are silent: a plan that grows on a plain click still
    /// sorts, and still draws an arrow, and the reader only finds out that the table is
    /// ordered by two columns they cannot see. The number the header draws comes out of
    /// the same list, so this also pins the badge to the order that decides the rows.
    #[test]
    fn a_plain_click_picks_one_column_and_shift_adds_a_second() {
        let mut plan = SortPlan::none();
        plan.cycle(2, false);
        assert_eq!(plan.columns(), [Sort::ascending(2)]);
        assert_eq!(plan.precedence(2), Some(1));
        assert_eq!(plan.direction(0), None);

        // A second plain click is a different column, not a second sort key.
        plan.cycle(0, false);
        assert_eq!(
            plan.columns(),
            [Sort::ascending(0)],
            "a plain click replaces the plan, so the table is never sorted by two \
             columns the reader did not ask for"
        );

        // Shift is the only way to reach a second column, and it keeps what is there.
        plan.cycle(1, true);
        assert_eq!(plan.columns(), [Sort::ascending(0), Sort::ascending(1)]);
        assert_eq!(plan.precedence(0), Some(1));
        assert_eq!(plan.precedence(1), Some(2));

        // Shift on a column already in the plan reverses it rather than adding it twice.
        plan.cycle(1, true);
        assert_eq!(plan.columns(), [Sort::ascending(0), Sort::descending(1)]);
        assert_eq!(plan.precedence(1), Some(2));

        // A plain click on the primary column reverses it, which is the native order for a
        // second click on a sorted header.
        plan.cycle(0, false);
        plan.cycle(0, false);
        assert_eq!(plan.columns(), [Sort::descending(0)]);

        // `UI-REDESIGN` §7 L6: the worst-first order the default view opens with holds until
        // the reader sorts by hand, and then it stops — a header click that leaves the rows
        // in the order they were already in is a header that looks broken. A `Shift`-click is
        // not taking over, so it keeps the grade in front.
        let mut severity = SortPlan::severity();
        severity.cycle(0, true);
        assert!(
            severity.is_severity_first(),
            "adding a key is not taking over the ordering"
        );
        severity.cycle(0, false);
        assert!(
            !severity.is_severity_first(),
            "a plain click is the reader choosing the order, so the default grade stops leading"
        );
        // And it is the two-state control a header is: the column is already the whole
        // plan, so the plain click reverses it rather than resetting it to ascending.
        assert_eq!(severity.columns(), [Sort::descending(0)]);
    }

    /// The two entry points of `UI-REDESIGN` §7 L5 B are one string, and the values in it
    /// are a set.
    ///
    /// A quoted value with a space in it is the case that breaks a rewrite built on
    /// whitespace: the token is one clause, and splitting it turns a replacement into an
    /// addition. The order is the other half — `status=Failed,Running` and
    /// `status=Running,Failed` are one filter, so they have to be one string, or a saved
    /// view and the box disagree about what it saved.
    #[test]
    fn the_clickable_filter_writes_the_query_string_and_nothing_else() {
        // A quoted value that is being replaced is replaced, not kept alongside.
        let rewritten = set_field_values(
            "name=\"my pod\" ns=prod",
            Field::Namespace,
            &["staging".to_owned()],
        );
        assert_eq!(rewritten, "name=\"my pod\" ns=staging");

        // Values are a set, so the order the reader ticked them in is not the string.
        assert_eq!(
            set_field_values(
                "",
                Field::Status,
                &["Running".to_owned(), "Failed".to_owned()]
            ),
            "status=Failed,Running"
        );
        assert_eq!(
            set_field_values(
                "status=Failed,Running",
                Field::Status,
                &["Running".to_owned(), "Failed".to_owned()]
            ),
            "status=Failed,Running"
        );

        // And what it writes is what the box then holds, which is the invariant the whole
        // of L5 B is about.
        let filter = Filter::parse(&rewritten).expect("the rewritten query parses");
        assert!(filter.matches_query(&rewritten));
        assert_eq!(filter.to_query(), rewritten);

        // Un-ticking the last value clears the clause rather than writing a clause the
        // grammar rejects, so the last untick is the same thing as `Clear`.
        assert_eq!(
            set_field_values("ns=prod status=Failed", Field::Status, &[]),
            "ns=prod"
        );
    }

    /// The Related block and the write confirmation ask the same question of the same
    /// object and have to get the same answer.
    ///
    /// This is `WRITE-OPS.md` §2.2 and `UI-REDESIGN.md` §7 L3 sharing one judgement, and
    /// the two live in different files, so nothing but a test stops a change to one of them
    /// from quietly disagreeing with the other. The interesting case is an object with two
    /// owner references: the Related block used to follow the first one while the
    /// confirmation quoted the controller, and `owner=` named a third answer.
    #[test]
    fn a_query_names_the_owner_the_confirmation_names() {
        let two_owners = Arc::new(
            serde_json::from_value(serde_json::json!({
                "metadata": {
                    "name": "api-7d2f9c6b8f-x7pvd",
                    "uid": "uid-pod",
                    "creationTimestamp": chrono::DateTime::from_timestamp(now_seconds(), 0)
                        .expect("now is a representable time")
                        .to_rfc3339(),
                    "ownerReferences": [
                        { "kind": "ReplicaSet", "name": "api-7d2f9c6b8f", "uid": "uid-rs", "controller": true },
                        { "kind": "Node", "name": "node-a", "uid": "uid-node" },
                    ],
                },
                "spec": { "nodeName": "node-a" },
            }))
            .expect("synthetic DynamicObject"),
        );

        // The controller is the one the confirmation names.
        let controller = crate::ops::controller_of(&two_owners).expect("a controller");
        assert_eq!(controller.name, "api-7d2f9c6b8f");
        // `owner=` and the Related block's followable row name the same uid.
        assert_eq!(
            Field::Owner.value_at(&two_owners, now_seconds()),
            Value::Text("uid-rs"),
            "the query has to land on the object the confirmation says will recreate it"
        );
        let related = relations(&two_owners);
        let controller_row = related
            .iter()
            .find(|row| row.is_controller)
            .expect("the controller is a Related row");
        assert_eq!(controller_row.name, controller.name);
        assert_eq!(controller_row.query, "owner=uid-rs");
        assert!(recreation(&two_owners).is_managed());

        // An owner reference with no `controller` key is lineage, not a promise, so it
        // counts as an owner and not as a controller — and the Related block says which.
        let uncontrolled = Arc::new(
            serde_json::from_value(serde_json::json!({
                "metadata": {
                    "name": "my-debug-pod",
                    "uid": "uid-debug",
                    "ownerReferences": [{ "kind": "Node", "name": "node-a", "uid": "uid-node" }],
                },
            }))
            .expect("synthetic DynamicObject"),
        );
        assert!(crate::ops::controller_of(&uncontrolled).is_none());
        assert_eq!(
            recreation(&uncontrolled),
            Recreation::Unmanaged { owners: 1 }
        );
        assert!(
            relations(&uncontrolled)
                .iter()
                .all(|row| !row.is_controller)
        );
    }

    /// The Apply preview's diff (`WRITE-OPS.md` §3.1, W8) on the edit the mockup draws.
    ///
    /// Two things here are silent failures: a field-level diff that reports the server's
    /// own bookkeeping is a four-hundred-line preview of a two-field edit, and a Secret's
    /// diff that prints its value is a credential in a panel somebody is about to
    /// screenshot. Both are asserted because neither fails any other test.
    #[test]
    fn objects_missing_name_uid_namespace_do_not_panic() {
        let objects = vec![bare_object(), object("named", None, None, &[], None)];

        let filtered = build_snapshot(
            objects.clone(),
            &[name_column()],
            &Filter::parse("no-such-name").expect("the filter parses"),
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

    /// A pod that carries every field the grammar can name, so one row answers
    /// every clause and a query's result is one word per clause.
    ///
    /// The creation timestamp is an hour ago, so `age>0s` holds of every pod here
    /// and `age>99d` holds of none — a pair that only means something if the pods
    /// are given an age at all.
    fn query_pod(name: &str, namespace: &str, status: &str, restarts: i64) -> Arc<DynamicObject> {
        // `app` follows the name, so a label clause can exclude a pod the way a
        // name clause cannot, and `team` is on all three, so a forced full-text
        // search has a label value that is in no pod's name.
        let app = if name.starts_with("web") {
            "web"
        } else {
            "api"
        };
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "metadata": {
                    "name": name,
                    "namespace": namespace,
                    "uid": format!("uid-{name}"),
                    "creationTimestamp": chrono::DateTime::from_timestamp(now_seconds() - 3_600, 0)
                        .expect("an hour ago is a representable time")
                        .to_rfc3339(),
                    "labels": { "app": app, "team": "platform" },
                    "ownerReferences": [{ "uid": "owner-1" }],
                },
                "spec": { "nodeName": "node-a" },
                "status": {
                    "phase": status,
                    "containerStatuses": [
                        { "ready": true, "restartCount": restarts },
                    ],
                },
            }))
            .expect("synthetic DynamicObject"),
        )
    }

    /// Whether a query admits the pods it names, which is the whole contract of a
    /// clause: a table row is listed when the query holds of it.
    fn admits(query: &str, pods: &[(&str, &str, &str, i64)]) -> Vec<String> {
        let filter = Filter::parse(query).expect("the query parses");
        pods.iter()
            .filter(|pod| {
                filter
                    .preds
                    .iter()
                    .all(|pred| pred.matches(&query_pod(pod.0, pod.1, pod.2, pod.3)))
            })
            .map(|pod| pod.0.to_owned())
            .collect()
    }

    /// One table over the grammar and the errors, because a clause and a located
    /// error are both silent failures: a clause that stops matching drops a pod
    /// nobody can account for, and an error that loses its span points the reader
    /// at the wrong token. Every clause in the documented grammar and every way of
    /// getting one wrong is a row here rather than a test of its own.
    #[test]
    fn the_query_grammar_holds_and_its_errors_point_at_the_token() {
        let pods = [
            ("api-1", "prod", "Running", 0),
            ("api-2", "prod", "Failed", 4),
            ("web-1", "staging", "Pending", 1),
        ];

        // The clause, the rows that survive it, and why.
        let cases: &[(&str, &[&str], &str)] = &[
            ("ns=prod", &["api-1", "api-2"], "namespace, exact"),
            (
                "ns=prod,staging",
                &["api-1", "api-2", "web-1"],
                "several namespaces",
            ),
            (
                "status!=Running",
                &["api-2", "web-1"],
                "any known field, negated",
            ),
            ("restarts>3", &["api-2"], "a numeric comparison"),
            (
                "age>0s",
                &["api-1", "api-2", "web-1"],
                "a duration, in seconds",
            ),
            ("age>99d", &[], "a duration nothing is old enough for"),
            (
                "label:app=api",
                &["api-1", "api-2"],
                "a label selector, on the existing Selector",
            ),
            (
                "label:app=web",
                &["web-1"],
                "the same selector on another value",
            ),
            (
                "label:app=api label:team=platform",
                &["api-1", "api-2"],
                "two label clauses are a conjunction",
            ),
            (
                "label:app=api,web",
                &["api-1", "api-2", "web-1"],
                "one label key, several values",
            ),
            ("label:!app", &[], "every pod here has an app label"),
            (
                "label:app",
                &["api-1", "api-2", "web-1"],
                "a bare label key asks for its presence",
            ),
            ("\"api-1\"", &["api-1"], "quotes mean the whole name"),
            (
                "api",
                &["api-1", "api-2"],
                "a bare word is a name substring",
            ),
            ("!api", &["web-1"], "negation"),
            (
                "q platform",
                &["api-1", "api-2", "web-1"],
                "q forces full text, so a label value is searchable when the name is not",
            ),
            (
                "q nosuchthing",
                &[],
                "and a forced search that matches nothing still admits nothing",
            ),
            (
                "ns=prod restarts>3",
                &["api-2"],
                "clauses are a conjunction",
            ),
            (
                "ns=prod !status=Failed",
                &["api-1"],
                "negation applies to the clause it prefixes",
            ),
            ("name=api-1", &["api-1"], "a field the reader spells out"),
            (
                "owner=owner-1",
                &["api-1", "api-2", "web-1"],
                "owner is the C4 filter, and it matches the owner reference",
            ),
            ("node=node-a", &["api-1", "api-2", "web-1"], "a spec field"),
            (
                "ready>=1",
                &["api-1", "api-2", "web-1"],
                "a counted field compares numerically",
            ),
        ];
        for (query, expected, why) in cases {
            assert_eq!(admits(query, &pods), *expected, "{query}: {why}");
        }

        // And the ways of getting a clause wrong, each with the token and what was
        // expected. An error that cannot say which token is wrong is an error the
        // reader cannot fix.
        let errors: &[(&str, &str, &str)] = &[
            (
                "staus=Running",
                "staus=Running",
                "unknown field `staus`. Known fields: ns, name, status, restarts, age, node, ready, owner, severity",
            ),
            (
                "age>soon",
                "age>soon",
                "expected a duration such as `30s`, `5m`, `2h` or `3d`",
            ),
            ("restarts>many", "restarts>many", "expected a number"),
            ("ns=", "ns=", "expected a value after `ns=`"),
            ("label:app=", "label:app=", "expected a value after `=`"),
            (
                "label:",
                "label:",
                "expected a label after `label:`, such as `label:app=api`",
            ),
            (
                "ns=prod \"unclosed",
                "\"unclosed",
                "this quote is never closed",
            ),
        ];
        for (query, token, expected) in errors {
            let error = Filter::parse(query).expect_err(&format!("{query} must not parse"));
            assert_eq!(&error.token, token, "{query}: the error names the token");
            assert_eq!(error.expected, *expected, "{query}: what was expected");
            // The span is what the query box underlines, so it has to cover the
            // token inside the source and nothing else.
            let slice = error.span.slice(query);
            assert_eq!(slice, *token, "{query}: the span covers the token");
        }

        // A pod that reports nothing is not dropped by a clause about a value it
        // does not have: `status!=Running` has to be true of it, and `status=Failed`
        // false, or the query silently deletes the rows with the least information.
        let bare = bare_object();
        let running = query_pod("api-1", "prod", "Running", 0);
        let not_running = Filter::parse("status!=Running").expect("parses");
        let failed = Filter::parse("status=Failed").expect("parses");
        assert!(not_running.preds.iter().all(|pred| pred.matches(&bare)));
        assert!(!failed.preds.iter().all(|pred| pred.matches(&bare)));
        assert!(!not_running.preds.iter().all(|pred| pred.matches(&running)));
    }

    /// The two entry points are one thing, and this is what makes them one: the
    /// filter the query box parsed and the string the column-header popover writes
    /// back are the same filter. Without this the typed form and the clickable
    /// form would each keep their own state, and a reader who copied one into the
    /// other would get a different table.
    #[test]
    fn a_filter_renders_back_into_the_query_it_came_from() {
        for query in [
            "",
            "ns=prod",
            "ns=prod,staging",
            "status!=Running",
            "restarts>3",
            "age>5m",
            "age>1h30m",
            "label:app=api",
            "label:!tier",
            "label:app=api,web",
            "\"exact name\"",
            "api",
            "!api",
            "q app",
            "ns=prod restarts>3",
            "!status=Failed",
            "name=api-1",
        ] {
            let filter = Filter::parse(query).unwrap_or_else(|error| {
                panic!("{query} must parse, but it said {}", error.message())
            });
            let rendered = filter.to_query();
            let reparsed = Filter::parse(&rendered)
                .unwrap_or_else(|error| panic!("{rendered} must re-parse: {}", error.message()));
            assert!(
                filter.matches_query(&rendered),
                "{query} rendered as `{rendered}`, which is a different filter"
            );
            assert_eq!(
                reparsed.to_query(),
                rendered,
                "{query}: rendering has to be stable, or the box would rewrite the \
                 reader's query on every keystroke"
            );
        }
    }
}
