//! Defines resource event sources, including a synthetic source.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use k8s_core::controller::{EVENT_BUFFER, StoreEvent, StoreOp};
use k8s_core::latency::{Latency, LatencyTier};
use kube_core::DynamicObject;
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedSender};

use super::EventSender;
use crate::session::{ObjectRef, OpsFuture};

pub(crate) const SOURCE_EVENT_BUFFER: usize = EVENT_BUFFER;

/// How many events one batch of this pipeline carries.
///
/// Both ends, deliberately: the bridge coalesces up to this many legacy events
/// and the host drains up to this many from the bounded channel, so a batch the
/// bridge sends is a batch the host takes in one pass. Two numbers that happen to
/// match would lose that, and `Vec::with_capacity` would be guessing.
pub(crate) const SOURCE_BATCH_SIZE: usize = 256;

/// Defines events sent by a resource source.
#[derive(Clone, Debug)]
pub enum SourceEvent {
    /// Starts a new watch and discards earlier objects.
    Init,
    /// Applies one store change.
    Store(StoreEvent),
    /// Marks the end of the initial list.
    InitDone,
    /// Marks cached objects that were sent after `Init`.
    /// `stale` is true after the cache TTL.
    CachePrimed {
        stale: bool,
        saved_at: SystemTime,
    },
    /// Reports a source failure.
    Error {
        reason: String,
    },
    LatencyUpdated(Latency),
    ControllerTier(LatencyTier),
}

#[derive(Default)]
pub(crate) struct SourceEventCoalescer {
    stores: Vec<StoreEvent>,
    indices: HashMap<String, usize>,
}

impl SourceEventCoalescer {
    pub(crate) fn push(&mut self, event: SourceEvent) -> Vec<SourceEvent> {
        match event {
            SourceEvent::Store(store) => {
                let Some(uid) = store.obj.metadata.uid.as_deref() else {
                    let mut output = self.flush();
                    output.push(SourceEvent::Store(store));
                    return output;
                };
                if let Some(index) = self.indices.get(uid) {
                    self.stores[*index] = store;
                } else {
                    self.indices.insert(uid.to_owned(), self.stores.len());
                    self.stores.push(store);
                }
                Vec::new()
            }
            event => {
                let mut output = self.flush();
                output.push(event);
                output
            }
        }
    }

    pub(crate) fn flush(&mut self) -> Vec<SourceEvent> {
        self.indices.clear();
        self.stores.drain(..).map(SourceEvent::Store).collect()
    }

    pub(crate) fn coalesce(events: impl IntoIterator<Item = SourceEvent>) -> Vec<SourceEvent> {
        let mut coalescer = Self::default();
        let mut output = Vec::new();
        for event in events {
            output.extend(coalescer.push(event));
        }
        output.extend(coalescer.flush());
        output
    }
}

/// Stops a resource source when dropped or cancelled.
pub trait Subscription: Send {
    fn cancel(&mut self);
}

struct BoundedSubscription {
    source: Option<Box<dyn Subscription>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Subscription for BoundedSubscription {
    fn cancel(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut source) = self.source.take() {
            source.cancel();
        }
    }
}

impl Drop for BoundedSubscription {
    fn drop(&mut self) {
        self.cancel();
        if self.join.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
    }
}

/// Defines a table data source.
pub trait ResourceSource: Send {
    fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription>;

    fn subscribe_bounded(&mut self, events: mpsc::Sender<SourceEvent>) -> Box<dyn Subscription> {
        let (legacy_events, mut legacy_receiver) = mpsc::unbounded_channel();
        let mut source = self.subscribe(legacy_events);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let error_events = events.clone();
        let join = thread::Builder::new()
            .name("resource-source-bridge".to_owned())
            .spawn(move || {
                let mut coalescer = SourceEventCoalescer::default();
                while !thread_stop.load(Ordering::Relaxed) {
                    let Some(event) = legacy_receiver.blocking_recv() else {
                        break;
                    };
                    let mut batch = coalescer.push(event);
                    let mut drained = 1;
                    while drained < SOURCE_BATCH_SIZE {
                        match legacy_receiver.try_recv() {
                            Ok(next) => {
                                batch.extend(coalescer.push(next));
                                drained += 1;
                            }
                            Err(mpsc::error::TryRecvError::Empty)
                            | Err(mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                    batch.extend(coalescer.flush());
                    for event in batch {
                        if events.blocking_send(event).is_err() {
                            return;
                        }
                    }
                }
                for event in coalescer.flush() {
                    if events.blocking_send(event).is_err() {
                        break;
                    }
                }
            });
        match join {
            Ok(join) => Box::new(BoundedSubscription {
                source: Some(source),
                stop,
                join: Some(join),
            }),
            Err(_) => {
                source.cancel();
                let _ = error_events.try_send(SourceEvent::Error {
                    reason:
                        "The resource source bridge did not start. Refresh the view to try again."
                            .to_owned(),
                });
                Box::new(BoundedSubscription {
                    source: None,
                    stop,
                    join: None,
                })
            }
        }
    }
}

/// Runs mutating resource operations for the table view.
pub trait ObjectOps: 'static {
    /// Deletes an object. `Ok(())` means the server accepted the request.
    fn delete(&self, object: ObjectRef) -> OpsFuture<()>;

    /// Changes the replica count.
    fn scale(&self, object: ObjectRef, replicas: i32) -> OpsFuture<()>;

    /// Starts a rolling restart.
    fn restart(&self, object: ObjectRef) -> OpsFuture<()>;
}

/// Controls synthetic updates from `FakeSource`.
#[derive(Clone, Debug)]
pub struct ChurnHandle(Arc<AtomicBool>);

impl ChurnHandle {
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(AtomicBool::new(enabled)))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }

    pub fn toggle(&self) -> bool {
        let next = !self.is_enabled();
        self.set(next);
        next
    }
}

impl Default for ChurnHandle {
    fn default() -> Self {
        Self::new(true)
    }
}

#[derive(Clone, Debug)]
pub struct FakeSourceConfig {
    pub pod_count: usize,
    pub seed: u64,
    /// Sets the delay between update batches.
    pub tick: Duration,
    /// Sets the number of added, modified, and deleted objects per batch.
    pub adds: usize,
    pub modifies: usize,
    pub deletes: usize,
    /// Sends an error after the given number of post-list ticks.
    pub fail_after_ticks: Option<u64>,
}

impl Default for FakeSourceConfig {
    fn default() -> Self {
        Self {
            pod_count: 10_000,
            seed: 0x5EED_1234,
            tick: Duration::from_millis(150),
            adds: 8,
            modifies: 30,
            deletes: 8,
            fail_after_ticks: None,
        }
    }
}

/// Sends a synthetic Pod list and later update batches.
pub struct FakeSource {
    config: FakeSourceConfig,
    churn: ChurnHandle,
}

impl FakeSource {
    pub fn new(config: FakeSourceConfig, churn: ChurnHandle) -> Self {
        Self { config, churn }
    }
}

impl FakeSource {
    fn subscribe_with_sender(&mut self, events: EventSender) -> Box<dyn Subscription> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let config = self.config.clone();
        let churn = self.churn.clone();
        let world = FakeWorld::new(config.pod_count, config.seed);
        let error_events = events.clone();
        let spawned = thread::Builder::new()
            .name("fake-pod-source".to_owned())
            .spawn(move || tick_loop(world, config, churn, thread_stop, events));
        match spawned {
            Ok(join) => Box::new(FakeSubscription {
                stop,
                join: Some(join),
            }),
            Err(error) => {
                // Keep internal error details out of the primary message.
                eprintln!("k8s-gpui: the pod data source did not start: {error}");
                let reason =
                    "The pod data source did not start. Refresh the view to try again.".to_owned();
                let _ = error_events.try_send(SourceEvent::Error { reason });
                Box::new(FakeSubscription { stop, join: None })
            }
        }
    }
}

impl ResourceSource for FakeSource {
    fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
        self.subscribe_with_sender(EventSender::Unbounded(events))
    }

    fn subscribe_bounded(&mut self, events: mpsc::Sender<SourceEvent>) -> Box<dyn Subscription> {
        self.subscribe_with_sender(EventSender::Bounded(events))
    }
}

struct FakeSubscription {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Subscription for FakeSubscription {
    fn cancel(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.cancel();
        // Never block the UI thread while joining the source thread.
        if self.join.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
    }
}

fn tick_loop(
    mut world: FakeWorld,
    config: FakeSourceConfig,
    churn: ChurnHandle,
    stop: Arc<AtomicBool>,
    events: EventSender,
) {
    if !events.send_blocking(SourceEvent::Init) {
        return;
    }
    for event in world.initial_events() {
        if !events.send_blocking(SourceEvent::Store(event)) {
            return;
        }
    }
    if !events.send_blocking(SourceEvent::InitDone) {
        return;
    }

    let mut ticks: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(config.tick);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(fail_after) = config.fail_after_ticks {
            ticks = ticks.saturating_add(1);
            if ticks >= fail_after {
                let _ = events.send_blocking(SourceEvent::Error {
                    reason: "The pod data source stopped. Refresh the view to try again."
                        .to_owned(),
                });
                return;
            }
        }
        if !churn.is_enabled() {
            continue;
        }
        for event in world.churn_batch(&config) {
            if !events.send_blocking(SourceEvent::Store(event)) {
                return;
            }
        }
    }
}

/// Owns the synthetic object set shared through events.
struct FakeWorld {
    rng: Lcg,
    counter: u64,
    objects: Vec<Arc<DynamicObject>>,
}

impl FakeWorld {
    fn new(count: usize, seed: u64) -> Self {
        let mut rng = Lcg::new(seed);
        let mut counter = 0;
        let objects = (0..count)
            .filter_map(|_| new_pod(&mut rng, &mut counter))
            .collect();
        Self {
            rng,
            counter,
            objects,
        }
    }

    fn initial_events(&self) -> impl Iterator<Item = StoreEvent> + '_ {
        self.objects.iter().map(|obj| StoreEvent {
            op: StoreOp::Apply,
            obj: Arc::clone(obj),
        })
    }

    fn churn_batch(&mut self, config: &FakeSourceConfig) -> Vec<StoreEvent> {
        let mut events = Vec::with_capacity(config.modifies + config.adds + config.deletes);
        for _ in 0..config.modifies {
            if let Some(event) = self.modify() {
                events.push(event);
            }
        }
        for _ in 0..config.deletes {
            if let Some(event) = self.delete() {
                events.push(event);
            }
        }
        for _ in 0..config.adds {
            if let Some(event) = self.add() {
                events.push(event);
            }
        }
        events
    }

    fn modify(&mut self) -> Option<StoreEvent> {
        if self.objects.is_empty() {
            return None;
        }
        let index = self.rng.below(self.objects.len() as u64) as usize;
        let current = self.objects.get(index)?;
        let mut value = serde_json::to_value(current.as_ref()).ok()?;
        mutate_pod(&mut value, &mut self.rng);
        let updated = Arc::new(serde_json::from_value::<DynamicObject>(value).ok()?);
        self.objects[index] = Arc::clone(&updated);
        Some(StoreEvent {
            op: StoreOp::Apply,
            obj: updated,
        })
    }

    fn delete(&mut self) -> Option<StoreEvent> {
        if self.objects.len() <= 1 {
            return None;
        }
        let index = self.rng.below(self.objects.len() as u64) as usize;
        let obj = self.objects.swap_remove(index);
        Some(StoreEvent {
            op: StoreOp::Delete,
            obj,
        })
    }

    fn add(&mut self) -> Option<StoreEvent> {
        let obj = new_pod(&mut self.rng, &mut self.counter)?;
        self.objects.push(Arc::clone(&obj));
        Some(StoreEvent {
            op: StoreOp::Apply,
            obj,
        })
    }
}

const SERVICES: [&str; 10] = [
    "coredns",
    "metrics-server",
    "ingress-nginx",
    "cert-manager",
    "prometheus",
    "grafana",
    "loki",
    "argocd",
    "cluster-autoscaler",
    "external-dns",
];

const NAMESPACES: [&str; 6] = [
    "default",
    "kube-system",
    "monitoring",
    "ingress-nginx",
    "cert-manager",
    "logging",
];

fn new_pod(rng: &mut Lcg, counter: &mut u64) -> Option<Arc<DynamicObject>> {
    *counter = counter.saturating_add(1);
    let service = SERVICES.get(rng.below(SERVICES.len() as u64) as usize)?;
    let namespace = NAMESPACES.get(rng.below(NAMESPACES.len() as u64) as usize)?;
    let name = format!("{service}-{:08x}", rng.next_u64() as u32);
    let uid = format!("{name}-{counter}");
    let image = format!(
        "registry.k8s.io/{service}:v1.{}.{}",
        rng.below(24),
        rng.below(9)
    );
    let created = Timestamp::now()
        .as_second()
        .saturating_sub(rng.below(400 * 86_400) as i64);
    let created = Timestamp::from_second(created).ok()?;
    let ready = rng.below(10) != 0;
    let restarts = rng.below(6) as i64;
    let status = pod_status(ready, restarts, &image, service, rng);
    let value = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": uid,
            "creationTimestamp": created.to_string(),
            "resourceVersion": rng.below(10_000_000).to_string(),
            "labels": {
                "app": service,
                "tier": if rng.below(2) == 0 { "frontend" } else { "backend" },
            },
        },
        "spec": {
            "nodeName": format!("node-{:02}.cluster.local", rng.below(24)),
            "containers": [{
                "name": service,
                "image": image,
                "ports": [{ "containerPort": match rng.below(4) { 0 => 80, 1 => 443, 2 => 8080, _ => 9090 } }],
            }],
        },
        "status": status,
    });
    serde_json::from_value(value).ok().map(Arc::new)
}

fn pod_status(ready: bool, restarts: i64, image: &str, name: &str, rng: &mut Lcg) -> Value {
    let state = if ready {
        json!({ "running": {} })
    } else if restarts > 3 {
        json!({ "waiting": { "reason": "CrashLoopBackOff" } })
    } else {
        json!({ "waiting": { "reason": "ContainerCreating" } })
    };
    json!({
        "phase": if ready { "Running" } else { "Pending" },
        "podIP": format!("10.244.{}.{}", rng.below(64), 1 + rng.below(254)),
        "containerStatuses": [{
            "name": name,
            "ready": ready,
            "restartCount": restarts,
            "image": image,
            "state": state,
        }],
    })
}

/// Changes mutable Pod fields while preserving the UID.
fn mutate_pod(value: &mut Value, rng: &mut Lcg) {
    let restarts = value
        .pointer("/status/containerStatuses/0/restartCount")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .saturating_add(rng.below(2) as i64);
    let ready = rng.below(20) != 0;
    let image = value
        .pointer("/spec/containers/0/image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let name = value
        .pointer("/spec/containers/0/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(status) = value.get_mut("status") {
        *status = pod_status(ready, restarts, &image, &name, rng);
    }
    if let Some(resource_version) = value.pointer_mut("/metadata/resourceVersion") {
        *resource_version = json!(rng.below(10_000_000).to_string());
    }
}

/// Provides deterministic synthetic data with a small PRNG.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(obj: &DynamicObject) -> String {
        obj.metadata.uid.clone().unwrap_or_default()
    }

    fn object(uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({ "metadata": { "uid": uid, "name": uid } }))
                .expect("synthetic object"),
        )
    }

    fn apply(uid: &str) -> StoreEvent {
        StoreEvent {
            op: StoreOp::Apply,
            obj: object(uid),
        }
    }

    fn delete(uid: &str) -> StoreEvent {
        StoreEvent {
            op: StoreOp::Delete,
            obj: object(uid),
        }
    }

    #[test]
    fn coalesces_store_events_without_moving_boundaries() {
        let events = SourceEventCoalescer::coalesce([
            SourceEvent::Init,
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(delete("uid-1")),
            SourceEvent::Store(apply("uid-2")),
            SourceEvent::InitDone,
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(delete("uid-1")),
        ]);

        assert_eq!(events.len(), 5);
        assert!(matches!(&events[0], SourceEvent::Init));
        assert!(matches!(
            &events[1],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-1")
        ));
        assert!(matches!(
            &events[2],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-2")
        ));
        assert!(matches!(&events[3], SourceEvent::InitDone));
        assert!(matches!(
            &events[4],
            SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj,
            }) if obj.metadata.uid.as_deref() == Some("uid-1")
        ));
    }

    #[test]
    fn initial_world_has_requested_pods_with_unique_uids() {
        let world = FakeWorld::new(200, 7);
        let events: Vec<StoreEvent> = world.initial_events().collect();
        assert_eq!(events.len(), 200);
        let uids: std::collections::HashSet<String> =
            events.iter().map(|event| uid(&event.obj)).collect();
        assert_eq!(uids.len(), 200, "UIDs must be unique");
        assert!(events.iter().all(|event| event.op == StoreOp::Apply));
    }

    #[test]
    fn churn_batch_mixes_ops_and_keeps_world_consistent() {
        let mut world = FakeWorld::new(200, 11);
        let config = FakeSourceConfig {
            pod_count: 200,
            tick: Duration::from_millis(1),
            ..FakeSourceConfig::default()
        };

        let mut seen_apply = false;
        let mut seen_delete = false;
        for _ in 0..5 {
            let events = world.churn_batch(&config);
            assert!(!events.is_empty());

            let deleted_in_batch: std::collections::HashSet<String> = events
                .iter()
                .filter(|event| event.op == StoreOp::Delete)
                .map(|event| uid(&event.obj))
                .collect();
            let alive: std::collections::HashSet<String> =
                world.objects.iter().map(|obj| uid(obj)).collect();

            for event in &events {
                match event.op {
                    StoreOp::Apply => {
                        seen_apply = true;
                        let uid = uid(&event.obj);
                        assert!(
                            alive.contains(&uid) || deleted_in_batch.contains(&uid),
                            "Apply objects must remain in the world or be deleted in the same batch"
                        );
                    }
                    StoreOp::Delete => {
                        seen_delete = true;
                        assert!(
                            !alive.contains(&uid(&event.obj)),
                            "Delete removes the UID from the world"
                        );
                    }
                }
            }
            assert_eq!(
                world.objects.len(),
                200,
                "Equal add and delete counts keep the row count stable"
            );
        }
        assert!(seen_apply && seen_delete);
    }

    #[test]
    fn modify_keeps_world_membership() {
        let mut world = FakeWorld::new(4, 3);
        let before: Vec<String> = world.objects.iter().map(|obj| uid(obj)).collect();
        for _ in 0..40 {
            let events = world.churn_batch(&FakeSourceConfig {
                pod_count: 4,
                adds: 0,
                modifies: 4,
                deletes: 0,
                tick: Duration::from_millis(1),
                seed: 0,
                fail_after_ticks: None,
            });
            assert!(events.iter().all(|event| event.op == StoreOp::Apply));
        }
        let after: std::collections::HashSet<String> =
            world.objects.iter().map(|obj| uid(obj)).collect();
        for uid in before {
            assert!(
                after.contains(&uid),
                "Updates do not change object membership"
            );
        }
    }

    #[test]
    fn lcg_is_deterministic() {
        let mut first = Lcg::new(42);
        let mut second = Lcg::new(42);
        assert_eq!(first.next_u64(), second.next_u64());
        assert_ne!(Lcg::new(1).next_u64(), Lcg::new(2).next_u64());
    }

    #[test]
    fn bounded_bridge_coalesces_legacy_store_events() {
        struct LegacySubscription {
            _events: UnboundedSender<SourceEvent>,
        }

        impl Subscription for LegacySubscription {
            fn cancel(&mut self) {}
        }

        struct LegacySource {
            events: Vec<SourceEvent>,
        }

        impl ResourceSource for LegacySource {
            fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
                for event in self.events.drain(..) {
                    events.send(event).expect("legacy receiver is alive");
                }
                Box::new(LegacySubscription { _events: events })
            }
        }

        let source_events = vec![
            SourceEvent::Init,
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(delete("uid-1")),
            SourceEvent::Store(apply("uid-2")),
            SourceEvent::InitDone,
            SourceEvent::Store(apply("uid-1")),
            SourceEvent::Store(delete("uid-1")),
        ];
        let (events, mut receiver) = mpsc::channel(4);
        let mut source = LegacySource {
            events: source_events,
        };
        let subscription = source.subscribe_bounded(events);

        assert!(matches!(receiver.blocking_recv(), Some(SourceEvent::Init)));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj,
            })) if obj.metadata.uid.as_deref() == Some("uid-1")
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj,
            })) if obj.metadata.uid.as_deref() == Some("uid-2")
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::InitDone)
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj,
            })) if obj.metadata.uid.as_deref() == Some("uid-1")
        ));
        drop(subscription);
    }

    #[test]
    fn bounded_fake_source_delivers_initial_list() {
        let (events, mut receiver) = mpsc::channel(8);
        let mut source = FakeSource::new(
            FakeSourceConfig {
                pod_count: 2,
                tick: Duration::from_millis(1),
                ..FakeSourceConfig::default()
            },
            ChurnHandle::new(false),
        );
        let subscription = source.subscribe_bounded(events);
        assert!(matches!(receiver.blocking_recv(), Some(SourceEvent::Init)));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                ..
            }))
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                ..
            }))
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(SourceEvent::InitDone)
        ));
        drop(subscription);
    }
}
