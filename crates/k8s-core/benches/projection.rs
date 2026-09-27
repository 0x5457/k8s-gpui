//! Measures the data-layer costs `UI-REDESIGN.md` §7 L1 names as N1 and N2: building a
//! snapshot out of 10,000 real `DynamicObject`s, and rebuilding it after one frame's worth
//! of watch events.
//!
//! The row set is the `docs/mockup/index.html` screen-1 cluster — 10,010 pods of which 110
//! are running and 9,888 were scheduled seconds ago — because a benchmark whose data has no
//! pending queue in it reports numbers for a cluster nobody has.
//!
//! Every case is timed over [`SAMPLES`] iterations and reported as p50/p95/p99 rather than a
//! mean: L1's budgets are percentile budgets, and a mean over ten samples is noise. The
//! whole point of the 10ms frame is the worst frame, not the average one.
//!
//! The benchmark reports rather than asserts. Every case but one is over the spec budget
//! today, and the reason is not in this layer — the cost is one projector call per cell per
//! row in `k8s-ui/src/table_view/columns.rs`, which is L1's third lever ("only recompute the
//! rows that are visible") and belongs to the crate that owns those closures. A red benchmark
//! in a shared workspace gets fixed by moving the threshold, which is the one thing worse
//! than a red benchmark. The numbers below are the record; `docs/` carries the verdicts.
//!
//! Run `mbx bench -p k8s-core --bench projection`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_core::projection::{
    CellValue, Column, Filter, SnapshotCache, Sort, SortKey, SortPlan, build_snapshot,
    build_snapshot_ordered,
};
use kube::core::DynamicObject;
use serde_json::Value;

/// Iterations per case. L1 requires at least 100 for p95/p99 to mean anything.
const SAMPLES: usize = 200;

/// Iterations run before timing starts, so the allocator is warm and the first case does not
/// pay for every other case's pages.
const WARMUP: usize = 5;

/// L1's budgets: 3ms to build a snapshot of 10,000 rows, 2ms for a frame's update path.
const BUILD_BUDGET: Duration = Duration::from_millis(3);
const UPDATE_BUDGET: Duration = Duration::from_millis(2);

/// The `Age` column, which a live table is never sorted by but which makes a worst case:
/// 9,888 of these rows carry the same age, so every comparison falls through to the
/// tie-break. A name sort has almost no ties and is cheaper.
const AGE_COLUMN: usize = 5;

/// Ten watch events in one frame.
const EVENTS_PER_FRAME: usize = 10;

fn main() {
    let objects = pods(10_010);
    let columns = pod_columns();
    println!("{} pods, {} columns\n", objects.len(), columns.len());
    let mut report = Report::default();

    // N1: the whole build, cold. This is what opening a table costs.
    report.measure("snapshot-cold", BUILD_BUDGET, |_| {
        let snapshot = build_snapshot_ordered(
            objects.clone(),
            &columns,
            &Filter::none(),
            &SortPlan::none(),
            1,
        );
        black_box(snapshot.rows.len());
    });

    // N1 with a query in the box: the filter path, which walks every field of every row.
    let query = Filter::parse("ns=prod status!=Running").expect("the query parses");
    report.measure("snapshot-cold-query", BUILD_BUDGET, |_| {
        let snapshot =
            build_snapshot_ordered(objects.clone(), &columns, &query, &SortPlan::none(), 1);
        black_box(snapshot.rows.len());
    });

    // N1 with a sort: the keys are taken out once and the rows permuted afterwards.
    report.measure("snapshot-cold-sorted", BUILD_BUDGET, |_| {
        let snapshot = build_snapshot(
            objects.clone(),
            &columns,
            &Filter::none(),
            Some(&Sort::descending(AGE_COLUMN)),
            1,
        );
        black_box(snapshot.rows.len());
    });

    // N1 in the order `UI-REDESIGN` L6 makes the default: worst first.
    report.measure("snapshot-cold-severity", BUILD_BUDGET, |_| {
        let snapshot = build_snapshot_ordered(
            objects.clone(),
            &columns,
            &Filter::none(),
            &SortPlan::severity(),
            1,
        );
        black_box(snapshot.rows.len());
    });

    // N2 before the cache existed, kept because it is the number the cache is measured
    // against: the same ten events, every cell rebuilt.
    let mut storm = objects.clone();
    report.measure("update-full", UPDATE_BUDGET, |iteration| {
        let changed = mutate(&mut storm, iteration);
        let snapshot = build_snapshot(
            changed,
            &columns,
            &Filter::none(),
            Some(&Sort::descending(AGE_COLUMN)),
            iteration as u64,
        );
        black_box(snapshot.rows.len());
    });

    // N2: the path a live table takes.
    let plan = SortPlan::of(Sort::descending(AGE_COLUMN));
    let mut storm = objects.clone();
    let mut cache = SnapshotCache::default();
    report.measure("update-cached", UPDATE_BUDGET, |iteration| {
        let changed = mutate(&mut storm, iteration);
        let snapshot = cache.rebuild(changed, &columns, &Filter::none(), &plan, iteration as u64);
        black_box(snapshot.rows.len());
    });

    // N2 with a query in the box, which is the case that has to walk every field of every
    // row on every rebuild.
    let mut storm = objects.clone();
    let mut cache = SnapshotCache::default();
    report.measure("update-cached-query", UPDATE_BUDGET, |iteration| {
        let changed = mutate(&mut storm, iteration);
        let snapshot = cache.rebuild(changed, &columns, &query, &plan, iteration as u64);
        black_box(snapshot.rows.len());
    });

    // N2 in the default order, where every row is graded against the clock on every rebuild.
    let mut storm = objects.clone();
    let mut cache = SnapshotCache::default();
    report.measure("update-cached-severity", UPDATE_BUDGET, |iteration| {
        let changed = mutate(&mut storm, iteration);
        let snapshot = cache.rebuild(
            changed,
            &columns,
            &Filter::none(),
            &SortPlan::severity(),
            iteration as u64,
        );
        black_box(snapshot.rows.len());
    });

    report.finish();
}

/// The measured cases and the budgets they are compared against.
#[derive(Default)]
struct Report {
    cases: usize,
    over: usize,
}

impl Report {
    fn measure(&mut self, label: &str, budget: Duration, mut body: impl FnMut(usize)) {
        for iteration in 0..WARMUP {
            body(iteration);
        }
        let mut samples = Vec::with_capacity(SAMPLES);
        for iteration in 0..SAMPLES {
            let started = Instant::now();
            body(iteration);
            samples.push(started.elapsed());
            black_box(iteration);
        }
        let (p50, p95, p99) = percentiles(&mut samples);
        let over = p99 > budget;
        self.cases += 1;
        self.over += usize::from(over);
        println!(
            "{label:24} p50 {:>7.3}ms  p95 {:>7.3}ms  p99 {:>7.3}ms  spec {:>3.0}ms  {}",
            millis(p50),
            millis(p95),
            millis(p99),
            budget.as_secs_f64() * 1000.0,
            if over { "over" } else { "ok" },
        );
    }

    fn finish(&self) {
        println!(
            "\n{} of {} cases over the L1 budget; see the module comment for where the cost is.",
            self.over, self.cases
        );
    }
}

fn percentiles(samples: &mut [Duration]) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let at = |quantile: usize| samples[quantile.min(samples.len() - 1)];
    (
        at(SAMPLES / 2),
        at(SAMPLES * 95 / 100),
        at(SAMPLES * 99 / 100),
    )
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// The mockup's cluster: mostly a queue that is moving, a few rows that are stuck, and the
/// handful that failed.
fn pods(count: usize) -> Vec<Arc<DynamicObject>> {
    let now = chrono::Utc::now();
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let namespace = ["prod", "staging", "kube-system"][index % 3];
        let (phase, age, restarts) = match index % 1000 {
            0..=109 => ("Running", 3_600, 0),
            110..=117 => ("Pending", 45, 0),
            118..=119 => ("Pending", 420, 0),
            120..=121 => ("Failed", 600, 7),
            _ => ("Pending", 4, 0),
        };
        let created = now - chrono::Duration::seconds(age);
        out.push(Arc::new(
            serde_json::from_value(pod_json(
                index,
                namespace,
                phase,
                &created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                restarts,
            ))
            .expect("synthetic DynamicObject"),
        ));
    }
    out
}

fn pod_json(index: usize, namespace: &str, phase: &str, created: &str, restarts: i64) -> Value {
    let app = ["api", "web", "worker", "coredns"][index % 4];
    let mut container =
        serde_json::json!({ "ready": phase == "Running", "restartCount": restarts });
    if phase == "Failed" {
        container["state"] = serde_json::json!({ "waiting": { "reason": "CrashLoopBackOff" } });
    }
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": format!("{app}-7f2b8c4d19-{index:05x}"),
            "namespace": namespace,
            "uid": format!("00000000-0000-0000-0000-{:012x}", index),
            "resourceVersion": format!("{}", 100_000 + index),
            "creationTimestamp": created,
            "labels": { "app": app, "tier": "backend" },
        },
        "spec": {
            "nodeName": format!("node-{}", index % 24),
            "containers": [{ "image": format!("registry.example.com/{app}:1.24.3") }],
        },
        "status": {
            "phase": phase,
            "podIP": format!("10.244.{}.{}", index / 250 % 250, index % 250),
            "containerStatuses": [container],
        },
    })
}

/// Applies one frame's watch events the way the store does: the object is replaced with a new
/// `Arc` carrying a new `resourceVersion`, and every other row keeps the identity it had.
fn mutate(objects: &mut [Arc<DynamicObject>], iteration: usize) -> Vec<Arc<DynamicObject>> {
    for step in 0..EVENTS_PER_FRAME {
        let index = (iteration * EVENTS_PER_FRAME + step) % objects.len();
        let mut data = objects[index].data.clone();
        let phase = if iteration.is_multiple_of(2) {
            "Running"
        } else {
            "Pending"
        };
        data["status"]["phase"] = Value::String(phase.to_owned());
        data["status"]["containerStatuses"][0]["restartCount"] = Value::from(iteration as i64);
        data["metadata"]["resourceVersion"] = Value::String(format!("{}", 200_000 + iteration));
        objects[index] = Arc::new(DynamicObject {
            data,
            metadata: objects[index].metadata.clone(),
            types: objects[index].types.clone(),
        });
    }
    objects.to_vec()
}

/// The seven Pod columns of `UI-SPEC` §10.2, doing the same work each one does in the app.
fn pod_columns() -> Vec<Column> {
    vec![
        Column::new("name", |obj| {
            cell(obj.metadata.name.as_deref().unwrap_or_default())
        }),
        Column::new("namespace", |obj| {
            cell(obj.metadata.namespace.as_deref().unwrap_or_default())
        }),
        Column::new("status", |obj| {
            let waiting = obj
                .data
                .get("status")
                .and_then(|status| status.get("containerStatuses"))
                .and_then(Value::as_array)
                .and_then(|statuses| {
                    statuses
                        .iter()
                        .find_map(|container| container.pointer("/state/waiting/reason"))
                        .and_then(Value::as_str)
                });
            cell(
                waiting
                    .or_else(|| obj.data.pointer("/status/phase").and_then(Value::as_str))
                    .unwrap_or_default(),
            )
        }),
        Column::new("ready", |obj| {
            let statuses = obj
                .data
                .get("status")
                .and_then(|status| status.get("containerStatuses"))
                .and_then(Value::as_array);
            let Some(statuses) = statuses else {
                return CellValue::text("—");
            };
            let ready = statuses
                .iter()
                .filter(|container| {
                    container
                        .get("ready")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .count() as i64;
            CellValue::new(
                format!("{ready}/{}", statuses.len()),
                SortKey::Int(ready * 100 / statuses.len().max(1) as i64),
            )
        }),
        Column::new("restarts", |obj| {
            let statuses = obj
                .data
                .get("status")
                .and_then(|status| status.get("containerStatuses"))
                .and_then(Value::as_array);
            let Some(statuses) = statuses else {
                return CellValue::text("—");
            };
            CellValue::number(
                statuses
                    .iter()
                    .filter_map(|container| container.get("restartCount"))
                    .filter_map(Value::as_i64)
                    .sum(),
            )
        }),
        Column::new("age", |obj| {
            let Some(created) = obj
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second())
            else {
                return CellValue::empty();
            };
            let age = now_seconds().saturating_sub(created).max(0);
            CellValue::new(format!("{age}s"), SortKey::Int(age))
        }),
        Column::new("node", |obj| {
            cell(
                obj.data
                    .pointer("/spec/nodeName")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        }),
    ]
}

fn cell(text: &str) -> CellValue {
    CellValue::text(text)
}
