//! Bridges the resource table state machine to GPUI.

use std::{
    cell::Cell,
    cmp::Ordering,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use gpui_kit::{AppContext as _, BackgroundExecutor, Context, Task};
use k8s_core::controller::{StoreEvent, StoreOp};
use k8s_core::latency::LatencyTier;
use k8s_core::machines::{ResourceTableMachine, ResourceTableState, TableEffect, TableEvent};
use k8s_core::projection::{Column, Filter, IndexSnapshot, Pred, Sort, build_snapshot};
use kube_core::DynamicObject;
use statig::blocking::StateMachine;
use statig::prelude::IntoStateMachineExt;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::columns::ResourceColumn;
use super::source::{
    ResourceSource, SOURCE_EVENT_BUFFER, SourceEvent, SourceEventCoalescer, Subscription,
};

/// Coalesces source updates for 150 ms.
pub const MERGE_INTERVAL: Duration = Duration::from_millis(150);
pub const PENDING_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

const SOURCE_BATCH_SIZE: usize = 256;
const PENDING_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Creates a new source for each watch start or retry.
pub type SourceFactory = Box<dyn Fn() -> Box<dyn ResourceSource>>;

/// Reports the startup placeholder reason. `shell` owns the constant, and it
/// is reachable because both modules live in the same crate.
fn is_startup_loading_reason(reason: &str) -> bool {
    reason == crate::shell::STARTUP_LOADING_REASON
}

/// Tracks local operation state until the watch confirms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingOp {
    Delete,
    Scale { replicas: i32 },
    Restart,
}

impl PendingOp {
    /// Returns the operation name used in notices.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Delete => "Delete",
            Self::Scale { .. } => "Scale",
            Self::Restart => "Restart",
        }
    }

    /// Returns the row badge label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Delete => "Terminating",
            Self::Scale { .. } | Self::Restart => "Pending",
        }
    }

    /// Explains that the row shows a local pending state.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Delete => "Delete requested. Waiting for the server to confirm.",
            Self::Scale { .. } => "Scale requested. Waiting for the server to confirm.",
            Self::Restart => "Restart requested. Waiting for the server to confirm.",
        }
    }
}

struct PendingEntry {
    op: PendingOp,
    /// Stores the resource version from before the request.
    resource_version: Option<String>,
    previous_restart_annotation: Option<String>,
    started_at: Instant,
}

struct RebuildRequest {
    filter: Filter,
    sort: Option<Sort>,
    relevance: bool,
    generation: Option<u64>,
    watch_epoch: u64,
    publish: bool,
}

/// Marks table rows that came from the disk cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedRows {
    /// Indicates that the cache is older than its TTL.
    pub stale: bool,
    pub saved_at: SystemTime,
}

/// Defines the user-facing table state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableStatus {
    Idle,
    Listing,
    Streaming,
    Paused,
    Stale(String),
    Failed(String),
    Stopped,
}

impl TableStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Listing => "Loading",
            Self::Streaming => "Live",
            Self::Paused => "Paused",
            Self::Stale(_) => "Stale",
            Self::Failed(_) => "Failed",
            Self::Stopped => "Stopped",
        }
    }

    fn from_state(state: &ResourceTableState) -> Self {
        match state {
            ResourceTableState::Idle {} => Self::Idle,
            ResourceTableState::Listing {} => Self::Listing,
            ResourceTableState::Streaming {} => Self::Streaming,
            ResourceTableState::Paused {} => Self::Paused,
            ResourceTableState::Failed { reason } => Self::Failed(reason.clone()),
            ResourceTableState::Stopped {} => Self::Stopped,
        }
    }
}

pub struct TableHost {
    machine: StateMachine<ResourceTableMachine>,
    effects: UnboundedReceiver<TableEffect>,
    columns: Vec<Column>,
    store: PodStore,
    source_factory: SourceFactory,
    subscription: Option<Box<dyn Subscription>>,
    /// Invalidates events from an older source.
    epoch: Rc<Cell<u64>>,
    merge: UpdateBuffer,
    /// Invalidates an older pending flush task.
    flush_epoch: u64,
    flush_scheduled: bool,
    clock: BackgroundExecutor,
    /// Defers rebuilds until the initial list ends.
    priming: bool,
    filter: Filter,
    relevance: bool,
    display_snapshot: Option<Arc<IndexSnapshot>>,
    _rebuild_task: Option<Task<()>>,
    active_rebuild: Option<u64>,
    pending_rebuild: Option<RebuildRequest>,
    next_rebuild_id: u64,
    /// Maps object UIDs to unconfirmed operations.
    pending: HashMap<String, PendingEntry>,
    pending_timer: Option<Task<()>>,
    pending_timer_epoch: u64,
    expired_pending: Vec<(String, PendingOp)>,
    opened_at: Instant,
    /// Measures the first source response.
    first_response: Option<Duration>,
    latency_rtt: Option<Duration>,
    controller_tier: Option<LatencyTier>,
    /// Measures time to the first applied snapshot.
    first_snapshot: Option<Duration>,
    watch_started_at: Option<Instant>,
    live_initialized: bool,
    stale_reason: Option<String>,
    /// When the watch that feeds these rows stopped answering.
    stale_since: Option<Instant>,
    cached: Option<CachedRows>,
    /// Keeps the last rows when a failure cancels the watch.
    keep_rows_on_cancel: bool,
}

impl TableHost {
    pub fn new(
        columns: &[ResourceColumn],
        source_factory: SourceFactory,
        cx: &mut Context<Self>,
    ) -> Self {
        let (effects_tx, effects_rx) = mpsc::unbounded_channel();
        let machine_columns: Vec<Column> =
            columns.iter().map(|column| column.column.clone()).collect();
        let machine = ResourceTableMachine::new(effects_tx).state_machine();
        let now = cx.background_executor().now();
        Self {
            machine,
            effects: effects_rx,
            columns: machine_columns,
            store: PodStore::default(),
            source_factory,
            subscription: None,
            epoch: Rc::new(Cell::new(0)),
            merge: UpdateBuffer::new(MERGE_INTERVAL, now),
            flush_epoch: 0,
            flush_scheduled: false,
            clock: cx.background_executor().clone(),
            priming: false,
            filter: Filter::default(),
            relevance: false,
            display_snapshot: None,
            _rebuild_task: None,
            active_rebuild: None,
            pending_rebuild: None,
            next_rebuild_id: 0,
            pending: HashMap::new(),
            pending_timer: None,
            pending_timer_epoch: 0,
            expired_pending: Vec::new(),
            opened_at: now,
            first_response: None,
            latency_rtt: None,
            controller_tier: None,
            first_snapshot: None,
            watch_started_at: None,
            live_initialized: false,
            stale_reason: None,
            stale_since: None,
            cached: None,
            keep_rows_on_cancel: false,
        }
    }

    /// Sends an event to the machine and runs its effects.
    pub fn dispatch(&mut self, event: TableEvent, cx: &mut Context<Self>) {
        if let TableEvent::FilterChanged { filter } = &event {
            self.filter = filter.clone();
        }
        if matches!(event, TableEvent::SnapshotUpdated { .. }) && self.first_snapshot.is_none() {
            self.first_snapshot = Some(self.clock.now().saturating_duration_since(self.opened_at));
        }
        self.machine.handle(&event);
        self.drain_effects(cx);
    }

    /// Records an unconfirmed operation for an object.
    pub fn mark_pending(&mut self, uid: &str, op: PendingOp) -> bool {
        if self.pending.contains_key(uid) {
            return false;
        }
        let current = self.store.get(uid);
        let resource_version = current.and_then(|obj| obj.metadata.resource_version.clone());
        let previous_restart_annotation = current.and_then(|obj| {
            obj.data
                .pointer("/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        });
        // A request the object already satisfies changes nothing, so the server
        // has no new state to report. Tracking it would leave the row waiting
        // for a confirmation that can never arrive, so the badge is skipped and
        // the caller still runs the request.
        if current.is_some_and(|object| already_satisfied(&op, object)) {
            return true;
        }
        self.pending.insert(
            uid.to_owned(),
            PendingEntry {
                op,
                resource_version,
                previous_restart_annotation,
                started_at: self.clock.now(),
            },
        );
        true
    }

    /// Returns how much longer the table waits for the server to confirm an
    /// operation. The row badge shows it so a slow request does not look stuck.
    pub fn pending_remaining(&self, uid: &str) -> Option<Duration> {
        let entry = self.pending.get(uid)?;
        Some(
            PENDING_OPERATION_TIMEOUT
                .saturating_sub(self.clock.now().saturating_duration_since(entry.started_at)),
        )
    }

    /// Removes an unconfirmed operation.
    pub fn resolve_pending(&mut self, uid: &str) -> Option<PendingOp> {
        self.pending.remove(uid).map(|entry| entry.op)
    }

    pub fn pending(&self, uid: &str) -> Option<&PendingOp> {
        self.pending.get(uid).map(|entry| &entry.op)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Every unconfirmed operation, with the time left before the table stops
    /// waiting, so a cell can draw its own badge without a second lookup.
    pub fn pending_states(&self) -> HashMap<String, (PendingOp, Duration)> {
        self.pending
            .iter()
            .map(|(uid, entry)| {
                (
                    uid.clone(),
                    (
                        entry.op.clone(),
                        PENDING_OPERATION_TIMEOUT.saturating_sub(
                            self.clock.now().saturating_duration_since(entry.started_at),
                        ),
                    ),
                )
            })
            .collect()
    }

    /// Names one unconfirmed operation, such as `Delete web-0`.
    pub fn pending_label(&self, uid: &str, op: &PendingOp) -> String {
        match self
            .store
            .get(uid)
            .and_then(|obj| obj.metadata.name.as_deref())
        {
            Some(name) => format!("{} {name}", op.kind()),
            None => format!("{} ({uid})", op.kind()),
        }
    }

    /// Names each unconfirmed operation, keeping the expiry notice specific.
    pub fn pending_labels(&self, entries: &[(String, PendingOp)]) -> Vec<String> {
        entries
            .iter()
            .map(|(uid, op)| self.pending_label(uid, op))
            .collect()
    }

    pub fn relevance(&self) -> bool {
        self.relevance
    }

    pub fn set_relevance(&mut self, relevance: bool) {
        self.relevance = relevance;
    }

    pub(crate) fn ensure_pending_check(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_empty() {
            self.pending_timer = None;
            return;
        }
        if self.pending_timer.is_some() {
            return;
        }
        self.pending_timer_epoch = self.pending_timer_epoch.wrapping_add(1);
        let epoch = self.pending_timer_epoch;
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PENDING_CHECK_INTERVAL).await;
            let _ = this.update(cx, |host, cx| {
                if host.pending_timer_epoch != epoch {
                    return;
                }
                host.pending_timer = None;
                host.expire_pending_at(host.clock.now());
                if !host.pending.is_empty() {
                    host.ensure_pending_check(cx);
                }
                cx.notify();
            });
        });
        self.pending_timer = Some(task);
    }

    pub fn expire_pending_at(&mut self, now: Instant) -> Vec<(String, PendingOp)> {
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, entry)| {
                now.saturating_duration_since(entry.started_at) >= PENDING_OPERATION_TIMEOUT
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        let mut result = Vec::with_capacity(expired.len());
        for uid in expired {
            if let Some(entry) = self.pending.remove(&uid) {
                result.push((uid, entry.op));
            }
        }
        self.expired_pending.extend(result.iter().cloned());
        result
    }

    pub fn take_expired_pending(&mut self) -> Vec<(String, PendingOp)> {
        std::mem::take(&mut self.expired_pending)
    }

    /// Returns the time to the first source response.
    pub fn first_response(&self) -> Option<Duration> {
        self.first_response
    }

    pub fn latency_rtt(&self) -> Option<Duration> {
        self.latency_rtt
    }

    pub fn controller_tier(&self) -> Option<LatencyTier> {
        self.controller_tier
    }

    pub fn first_snapshot(&self) -> Option<Duration> {
        self.first_snapshot
    }

    /// Returns cache metadata while cached rows are active.
    pub fn cached(&self) -> Option<CachedRows> {
        self.cached
    }

    /// How long ago the rows on screen were written to disk, and `None` when
    /// they are live rows.
    ///
    /// The cache is cleared the moment the live list lands, so this is the age of
    /// the *displayed* rows and nothing else — which is the only age a reader who
    /// is about to act on a row needs.
    pub fn cache_age(&self) -> Option<Duration> {
        self.cached.map(|cached| {
            SystemTime::now()
                .duration_since(cached.saved_at)
                .unwrap_or_default()
        })
    }

    /// How long the watch has been dead while rows are still on screen.
    ///
    /// `TableStatus::Stale` carries a reason and no clock, so without this a
    /// frozen table and a table that died a second ago are the same word. A reader
    /// deciding whether to trust the rows needs to know whether the freeze is
    /// seconds old or an hour.
    pub fn stale_age(&self) -> Option<Duration> {
        self.stale_since
            .map(|since| self.clock.now().saturating_duration_since(since))
    }

    pub fn is_high_latency(&self) -> bool {
        matches!(self.controller_tier, Some(LatencyTier::HighLatency))
    }

    pub fn status(&self) -> TableStatus {
        if let ResourceTableState::Failed { reason } = self.machine.state() {
            // The startup session sends a placeholder reason until the
            // kubeconfig lands. It is a loading state, so the first paint is a
            // skeleton instead of a full-screen error.
            if self.is_startup_loading() {
                return TableStatus::Listing;
            }
            // Rows on screen outrank the failure badge. Cached or live rows stay
            // readable, and `Retry` reconnects from the toolbar.
            if self.snapshot().is_some() {
                let reason = self.stale_reason.clone().unwrap_or_else(|| reason.clone());
                return TableStatus::Stale(reason);
            }
            return TableStatus::Failed(reason.clone());
        }
        if self.cached.is_some() && !self.live_initialized {
            return TableStatus::Listing;
        }
        TableStatus::from_state(self.machine.state())
    }

    /// Reports that the machine holds the startup placeholder reason, which
    /// means the session is still loading rather than broken.
    pub fn is_startup_loading(&self) -> bool {
        match self.machine.state() {
            ResourceTableState::Failed { reason } => is_startup_loading_reason(reason),
            _ => false,
        }
    }

    pub fn report_watch_error(&mut self, reason: String, cx: &mut Context<Self>) {
        // A failure next to visible rows degrades to Stale instead of Failed.
        self.stale_reason = self.snapshot().is_some().then(|| reason.clone());
        self.stale_since = self.stale_reason.is_some().then(|| self.clock.now());
        // CancelWatch must not throw away rows the user can still read.
        self.keep_rows_on_cancel = self.stale_reason.is_some();
        self.dispatch(TableEvent::StoreError { reason }, cx);
    }

    pub fn snapshot(&self) -> Option<Arc<IndexSnapshot>> {
        if !self.live_initialized
            && let Some(snapshot) = &self.display_snapshot
        {
            return Some(Arc::clone(snapshot));
        }
        self.machine.inner().snapshot().cloned()
    }

    pub fn sort(&self) -> Option<Sort> {
        self.machine.inner().sort()
    }

    pub fn row_count(&self) -> usize {
        self.snapshot().map_or(0, |snapshot| snapshot.rows.len())
    }

    /// Returns the unfiltered object count.
    pub fn total_count(&self) -> usize {
        self.store.len()
    }

    /// Restarts the source or retries a failed watch.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.status() {
            TableStatus::Failed(_) | TableStatus::Stale(_) => self.dispatch(TableEvent::Retry, cx),
            TableStatus::Idle | TableStatus::Stopped => self.dispatch(TableEvent::Start, cx),
            _ => {
                self.dispatch(TableEvent::Stop, cx);
                self.dispatch(TableEvent::Start, cx);
            }
        }
    }

    fn drain_effects(&mut self, cx: &mut Context<Self>) {
        while let Ok(effect) = self.effects.try_recv() {
            self.run_effect(effect, cx);
        }
    }

    fn run_effect(&mut self, effect: TableEffect, cx: &mut Context<Self>) {
        match effect {
            TableEffect::SpawnWatch => self.spawn_watch(cx),
            TableEffect::CancelWatch => {
                let keep_rows = self.keep_rows_on_cancel;
                self.cancel_watch(keep_rows);
            }
            TableEffect::RebuildSnapshot {
                filter,
                sort,
                generation,
            } => self.rebuild(filter, sort, generation, cx),
            TableEffect::Notify => cx.notify(),
        }
    }

    fn spawn_watch(&mut self, cx: &mut Context<Self>) {
        self.cancel_watch(false);
        self.live_initialized = false;
        self.stale_reason = None;
        self.stale_since = None;
        let epoch = self.epoch.get().wrapping_add(1);
        self.epoch.set(epoch);

        let mut source = (self.source_factory)();
        let (events_tx, mut events_rx) = mpsc::channel(SOURCE_EVENT_BUFFER);
        self.subscription = Some(source.subscribe_bounded(events_tx));
        self.store.clear();
        self.priming = true;
        self.merge.mark();
        // Keep the first connection timing across later retries.
        if self.watch_started_at.is_none() {
            self.watch_started_at = Some(self.clock.now());
        }

        let epoch_cell = Rc::clone(&self.epoch);
        let task = cx.spawn(async move |this, cx| {
            'outer: loop {
                if epoch_cell.get() != epoch {
                    break 'outer;
                }
                let mut batch = Vec::with_capacity(SOURCE_BATCH_SIZE);
                let mut disconnected = false;
                while batch.len() < SOURCE_BATCH_SIZE {
                    match events_rx.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                let batch = SourceEventCoalescer::coalesce(batch);
                if !batch.is_empty()
                    && this
                        .update(cx, |host, cx| {
                            for event in batch {
                                host.on_source_event(event, cx);
                                if epoch_cell.get() != epoch {
                                    break;
                                }
                            }
                        })
                        .is_err()
                {
                    break 'outer;
                }
                if disconnected {
                    if epoch_cell.get() == epoch
                        && this
                            .update(cx, |host, cx| {
                                host.report_watch_error(
                                    "The connection for live updates ended. Refresh the view to reconnect."
                                        .to_owned(),
                                    cx,
                                );
                            })
                            .is_err()
                    {
                        break 'outer;
                    }
                    break 'outer;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(25))
                    .await;
            }
        });
        task.detach();
    }

    /// Cancels the watch. `keep_rows` retains the last rows for a failure.
    fn cancel_watch(&mut self, keep_rows: bool) {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.flush_epoch = self.flush_epoch.wrapping_add(1);
        self.flush_scheduled = false;
        self.priming = false;
        self.keep_rows_on_cancel = false;
        if !keep_rows {
            self.cached = None;
            self.display_snapshot = None;
            self.pending_rebuild = None;
            self.latency_rtt = None;
            self.controller_tier = None;
        }
        if let Some(mut subscription) = self.subscription.take() {
            subscription.cancel();
        }
    }

    fn on_source_event(&mut self, event: SourceEvent, cx: &mut Context<Self>) {
        if self.first_response.is_none()
            && matches!(event, SourceEvent::Store(_) | SourceEvent::InitDone)
            && let Some(started) = self.watch_started_at
        {
            self.first_response = Some(self.clock.now().saturating_duration_since(started));
        }
        match event {
            SourceEvent::Init => {
                self.store.clear();
                self.live_initialized = false;
                self.priming = true;
                self.merge.mark();
                return;
            }
            SourceEvent::Store(event) => {
                self.store.apply(&event);
                if let Some(uid) = event.obj.metadata.uid.clone() {
                    self.converge_pending(&uid, &event);
                }
                self.merge.mark();
            }
            SourceEvent::InitDone => {
                self.reconcile_pending();
                // The live list is ready, so cached state no longer applies.
                self.live_initialized = true;
                self.cached = None;
                self.display_snapshot = None;
                self.priming = false;
                self.merge.flushed_at(self.clock.now());
                self.flush_epoch = self.flush_epoch.wrapping_add(1);
                self.flush_scheduled = false;
                self.dispatch(TableEvent::WatchInitDone, cx);
                return;
            }
            SourceEvent::CachePrimed { stale, saved_at } => {
                // Render cached rows immediately. The next live Init replaces them.
                self.live_initialized = false;
                self.cached = Some(CachedRows { stale, saved_at });
                // Start response timing after the cache, not before it.
                self.first_response = None;
                self.watch_started_at = Some(self.clock.now());
                self.merge.flushed_at(self.clock.now());
                self.flush_epoch = self.flush_epoch.wrapping_add(1);
                self.flush_scheduled = false;
                self.request_rebuild(self.filter.clone(), self.sort(), None, false, cx);
                cx.notify();
                return;
            }
            SourceEvent::LatencyUpdated(latency) => {
                self.latency_rtt = latency.rtt;
                cx.notify();
                return;
            }
            SourceEvent::ControllerTier(tier) => {
                self.controller_tier = Some(tier);
                cx.notify();
                return;
            }
            SourceEvent::Error { reason } => {
                self.report_watch_error(reason, cx);
                return;
            }
        }

        // Keep the store current while the machine applies paused-state rules.
        if !self.priming {
            if self.merge.should_flush_at(self.clock.now()) {
                self.flush_merge(cx);
            } else {
                // A pending timer handles the tail of a burst after the last event.
                self.schedule_flush(cx);
            }
        }
    }

    fn reconcile_pending(&mut self) {
        let pending: Vec<String> = self.pending.keys().cloned().collect();
        for uid in pending {
            let Some(object) = self.store.get(&uid).cloned() else {
                self.pending.remove(&uid);
                continue;
            };
            self.converge_pending(
                &uid,
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: object,
                },
            );
        }
    }

    /// Clears pending state when the watch confirms an operation.
    fn converge_pending(&mut self, uid: &str, event: &StoreEvent) {
        let Some(entry) = self.pending.get(uid) else {
            return;
        };
        let resource_changed = event.obj.metadata.resource_version != entry.resource_version;
        let converged = match (event.op, &entry.op) {
            (StoreOp::Delete, _) => true,
            (StoreOp::Apply, PendingOp::Delete) => event.obj.metadata.deletion_timestamp.is_some(),
            (StoreOp::Apply, PendingOp::Scale { replicas }) => {
                resource_changed
                    && event
                        .obj
                        .data
                        .pointer("/spec/replicas")
                        .and_then(|value| value.as_i64())
                        == Some(i64::from(*replicas))
            }
            (StoreOp::Apply, PendingOp::Restart) => {
                let restarted_at = event
                    .obj
                    .data
                    .pointer(
                        "/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt",
                    )
                    .and_then(|value| value.as_str());
                resource_changed
                    && restarted_at.is_some()
                    && restarted_at != entry.previous_restart_annotation.as_deref()
            }
        };
        if converged {
            self.pending.remove(uid);
        }
    }

    /// Rebuilds the snapshot and cancels an older flush task.
    fn flush_merge(&mut self, cx: &mut Context<Self>) {
        self.flush_epoch = self.flush_epoch.wrapping_add(1);
        self.flush_scheduled = false;
        self.merge.flushed_at(self.clock.now());
        self.dispatch(TableEvent::WatchInitDone, cx);
    }

    /// Schedules one trailing flush for the current merge window.
    fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        if self.flush_scheduled {
            return;
        }
        self.flush_scheduled = true;
        let epoch = self.flush_epoch;
        let delay = self.merge.interval.saturating_sub(
            self.clock
                .now()
                .saturating_duration_since(self.merge.last_flush),
        );
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |host, cx| {
                if host.flush_epoch == epoch {
                    host.on_flush_timer(cx);
                }
            });
        });
        task.detach();
    }

    fn on_flush_timer(&mut self, cx: &mut Context<Self>) {
        self.flush_scheduled = false;
        if self.priming || !self.merge.dirty {
            return;
        }
        if self.merge.should_flush_at(self.clock.now()) {
            self.flush_merge(cx);
        } else {
            // Reschedule if the clock woke the task before the window ended.
            self.schedule_flush(cx);
        }
    }

    fn request_rebuild(
        &mut self,
        filter: Filter,
        sort: Option<Sort>,
        generation: Option<u64>,
        publish: bool,
        cx: &mut Context<Self>,
    ) {
        let request = RebuildRequest {
            filter,
            sort,
            relevance: self.relevance,
            generation,
            watch_epoch: self.epoch.get(),
            publish,
        };
        if self.active_rebuild.is_some() {
            self.pending_rebuild = Some(request);
        } else {
            self.start_rebuild(request, cx);
        }
    }

    fn start_rebuild(&mut self, request: RebuildRequest, cx: &mut Context<Self>) {
        let RebuildRequest {
            filter,
            sort,
            relevance,
            generation,
            watch_epoch,
            publish,
        } = request;
        let rebuild_id = self.next_rebuild_id.wrapping_add(1);
        self.next_rebuild_id = rebuild_id;
        let snapshot_generation = generation.unwrap_or(rebuild_id);
        let items: Vec<Arc<DynamicObject>> = self.store.objects().collect();
        let columns = self.columns.clone();
        let build = cx.background_spawn(async move {
            build_table_snapshot(
                items,
                &columns,
                &filter,
                sort.as_ref(),
                relevance,
                snapshot_generation,
            )
        });
        self.active_rebuild = Some(rebuild_id);
        let task = cx.spawn(async move |this, cx| {
            let snapshot = build.await;
            let _ = this.update(cx, |host, cx| {
                host.finish_rebuild(rebuild_id, watch_epoch, publish, generation, snapshot, cx);
            });
        });
        self._rebuild_task = Some(task);
    }

    fn finish_rebuild(
        &mut self,
        rebuild_id: u64,
        watch_epoch: u64,
        publish: bool,
        generation: Option<u64>,
        snapshot: IndexSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.active_rebuild != Some(rebuild_id) {
            return;
        }
        self.active_rebuild = None;
        if self.epoch.get() == watch_epoch {
            let snapshot = Arc::new(snapshot);
            if publish && self.live_initialized {
                if let Some(generation) = generation {
                    self.dispatch(
                        TableEvent::SnapshotUpdated {
                            snapshot,
                            generation,
                        },
                        cx,
                    );
                }
            } else if !self.live_initialized
                && (self.store.len() > 0 || self.display_snapshot.is_none())
            {
                self.display_snapshot = Some(snapshot);
                if self.first_snapshot.is_none() {
                    self.first_snapshot =
                        Some(self.clock.now().saturating_duration_since(self.opened_at));
                }
                cx.notify();
            }
        }
        if let Some(next) = self.pending_rebuild.take()
            && next.watch_epoch == self.epoch.get()
        {
            if self.active_rebuild.is_none() {
                self.start_rebuild(next, cx);
            } else {
                self.pending_rebuild = Some(next);
            }
        }
    }

    fn rebuild(
        &mut self,
        filter: Filter,
        sort: Option<Sort>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.request_rebuild(filter, sort, Some(generation), self.live_initialized, cx);
    }
}

fn stable_object_order(left: &Arc<DynamicObject>, right: &Arc<DynamicObject>) -> Ordering {
    left.metadata
        .name
        .as_deref()
        .unwrap_or_default()
        .cmp(right.metadata.name.as_deref().unwrap_or_default())
        .then_with(|| {
            left.metadata
                .namespace
                .as_deref()
                .unwrap_or_default()
                .cmp(right.metadata.namespace.as_deref().unwrap_or_default())
        })
        .then_with(|| {
            left.metadata
                .uid
                .as_deref()
                .unwrap_or_default()
                .cmp(right.metadata.uid.as_deref().unwrap_or_default())
        })
}

fn build_table_snapshot(
    mut items: Vec<Arc<DynamicObject>>,
    columns: &[Column],
    filter: &Filter,
    sort: Option<&Sort>,
    relevance: bool,
    generation: u64,
) -> IndexSnapshot {
    if sort.is_none() {
        items.sort_by(stable_object_order);
    }
    // The name search is the one clause that is a *ranking* as well as a filter,
    // so it is taken out of the snapshot filter and applied by the ranker below.
    // Every other clause is a filter and nothing else.
    let query = filter
        .search_needle()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let mut base_filter = filter.clone();
    // Only the clause the ranker is using comes out. A second text clause stays
    // in, because dropping it would drop a filter the reader asked for.
    if let Some(index) = base_filter.preds.iter().position(|pred| {
        matches!(
            pred,
            Pred::Name { exact: false, .. } | Pred::FullText { .. }
        )
    }) {
        base_filter.preds.remove(index);
    }
    let snapshot = build_snapshot(items, columns, &base_filter, sort, generation);
    if query.is_empty() {
        return snapshot;
    }

    let ranked = {
        let names: Vec<&str> = snapshot
            .rows
            .iter()
            .map(|row| row.obj.metadata.name.as_deref().unwrap_or_default())
            .collect();
        k8s_core::fuzzy::rank(&query, names)
    };
    let mut source = snapshot.rows.into_iter().map(Some).collect::<Vec<_>>();
    let rows = if relevance {
        let mut rows = Vec::with_capacity(ranked.len());
        for ranked in ranked {
            rows.push(
                source[ranked.index]
                    .take()
                    .expect("fuzzy index out of range"),
            );
        }
        rows
    } else {
        let mut matches = vec![false; source.len()];
        for ranked in &ranked {
            matches[ranked.index] = true;
        }
        source
            .into_iter()
            .enumerate()
            .filter_map(|(index, row)| matches[index].then_some(row).flatten())
            .collect()
    };
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

/// Reports that the object already shows the state an operation asks for, so
/// the operation needs no confirmation from the watch.
fn already_satisfied(op: &PendingOp, object: &DynamicObject) -> bool {
    match op {
        PendingOp::Delete => object.metadata.deletion_timestamp.is_some(),
        PendingOp::Scale { replicas } => {
            object
                .data
                .pointer("/spec/replicas")
                .and_then(|value| value.as_i64())
                == Some(i64::from(*replicas))
        }
        // Restart writes a new timestamp, so the current object cannot show it.
        PendingOp::Restart => false,
    }
}

/// Mirrors source objects by UID for snapshot rebuilds.
#[derive(Default)]
struct PodStore {
    by_uid: HashMap<String, Arc<DynamicObject>>,
}

impl PodStore {
    fn apply(&mut self, event: &StoreEvent) {
        let Some(uid) = event.obj.metadata.uid.as_deref() else {
            return;
        };
        match event.op {
            StoreOp::Apply => {
                self.by_uid.insert(uid.to_owned(), Arc::clone(&event.obj));
            }
            StoreOp::Delete => {
                self.by_uid.remove(uid);
            }
        }
    }

    fn objects(&self) -> impl Iterator<Item = Arc<DynamicObject>> + '_ {
        self.by_uid.values().cloned()
    }

    fn get(&self, uid: &str) -> Option<&Arc<DynamicObject>> {
        self.by_uid.get(uid)
    }

    fn len(&self) -> usize {
        self.by_uid.len()
    }

    fn clear(&mut self) {
        self.by_uid.clear();
    }
}

/// Coalesces marked source updates for one interval.
pub(crate) struct UpdateBuffer {
    interval: Duration,
    last_flush: Instant,
    dirty: bool,
}

impl UpdateBuffer {
    pub(crate) fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            last_flush: now,
            dirty: false,
        }
    }

    pub(crate) fn mark(&mut self) {
        self.dirty = true;
    }

    fn should_flush_at(&self, now: Instant) -> bool {
        self.dirty && now.saturating_duration_since(self.last_flush) >= self.interval
    }

    fn flushed_at(&mut self, now: Instant) {
        self.last_flush = now;
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    // Source polling requires an explicit clock advance in tests. The test
    // harness already installs a theme, so there is nothing to set up.

    fn pump_source(cx: &mut TestAppContext) {
        cx.executor().advance_clock(Duration::from_millis(30));
        cx.run_until_parked();
    }

    use super::super::columns::pod_columns;
    use super::*;
    use crate::table_view::{ChurnHandle, FakeSource, FakeSourceConfig};
    use gpui_kit::TestAppContext;
    use k8s_core::projection::CellValue;
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedSender;

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

    fn named_object(name: &str, uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({ "metadata": { "name": name, "uid": uid } }))
                .expect("synthetic object"),
        )
    }

    fn name_column() -> Column {
        Column::new("name", |obj| {
            obj.metadata
                .name
                .as_deref()
                .map_or_else(CellValue::empty, CellValue::text)
        })
    }

    fn table_snapshot(values: &[(&str, &str)], query: &str, sort: Option<&Sort>) -> IndexSnapshot {
        let objects = values
            .iter()
            .map(|(name, uid)| named_object(name, uid))
            .collect();
        let columns = [name_column()];
        let filter = Filter::parse(query).expect("the query parses");
        build_table_snapshot(objects, &columns, &filter, sort, true, 1)
    }

    #[gpui_kit::test]
    fn controller_tier_controls_high_latency_state(cx: &mut TestAppContext) {
        let factory: SourceFactory = Box::new(|| {
            Box::new(FakeSource::new(
                FakeSourceConfig::default(),
                ChurnHandle::new(false),
            )) as Box<dyn ResourceSource>
        });
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| {
            host.on_source_event(
                SourceEvent::LatencyUpdated(k8s_core::latency::Latency {
                    rtt: Some(Duration::from_millis(220)),
                    loss_rate: 0.0,
                    probes: 1,
                    failures: 0,
                }),
                cx,
            );
            assert_eq!(host.latency_rtt(), Some(Duration::from_millis(220)));
            host.on_source_event(SourceEvent::ControllerTier(LatencyTier::Local), cx);
            assert!(!host.is_high_latency());
            host.on_source_event(SourceEvent::ControllerTier(LatencyTier::HighLatency), cx);
            assert!(host.is_high_latency());
        });
    }

    #[gpui_kit::test]
    fn pending_ops_converge_only_on_server_confirmation(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });

        // A new resource version alone does not confirm a scale request: the
        // object must also show the requested replica count.
        host.update(cx, |host, _| {
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: scaled_object("uid-1", "10", 1),
            });
            host.mark_pending("uid-1", PendingOp::Scale { replicas: 3 });
        });
        host.update(cx, |host, _| {
            host.converge_pending(
                "uid-1",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: scaled_object("uid-1", "10", 1),
                },
            );
        });
        assert_eq!(
            cx.update(|cx| host.read(cx).pending("uid-1").cloned()),
            Some(PendingOp::Scale { replicas: 3 })
        );

        host.update(cx, |host, _| {
            host.converge_pending(
                "uid-1",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: scaled_object("uid-1", "11", 1),
                },
            );
        });
        assert_eq!(
            cx.update(|cx| host.read(cx).pending("uid-1").cloned()),
            Some(PendingOp::Scale { replicas: 3 }),
            "the count still does not match the request"
        );

        host.update(cx, |host, _| {
            host.converge_pending(
                "uid-1",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: scaled_object("uid-1", "12", 3),
                },
            );
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 0);
    }

    // A request that changes nothing would otherwise hold the badge until the
    // deadline and then report an unknown result for a state the table already
    // shows.
    #[gpui_kit::test]
    fn a_request_the_object_already_satisfies_is_not_tracked(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: scaled_object("uid-1", "10", 3),
            });
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: terminating("uid-2"),
            });
            assert!(
                host.mark_pending("uid-1", PendingOp::Scale { replicas: 3 }),
                "the request is not blocked"
            );
            assert!(
                host.mark_pending("uid-2", PendingOp::Delete),
                "a delete on a terminating object is not blocked either"
            );
        });

        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 0);
    }

    // The row badge counts down to the deadline instead of waiting silently.
    #[gpui_kit::test]
    fn pending_rows_report_the_time_left_before_the_deadline(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.mark_pending("uid-1", PendingOp::Restart);
        });
        let started = cx.update(|cx| host.read(cx).pending_remaining("uid-1"));
        assert_eq!(started, Some(PENDING_OPERATION_TIMEOUT));

        cx.executor().advance_clock(Duration::from_secs(5));
        let remaining = cx.update(|cx| host.read(cx).pending_remaining("uid-1"));
        assert_eq!(
            remaining,
            Some(PENDING_OPERATION_TIMEOUT - Duration::from_secs(5))
        );
        assert_eq!(
            cx.update(|cx| host.read(cx).pending_remaining("uid-2")),
            None
        );
    }

    #[gpui_kit::test]
    fn pending_delete_converges_on_terminating_apply_or_delete_event(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.mark_pending("uid-1", PendingOp::Delete);
            host.mark_pending("uid-2", PendingOp::Delete);
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 2);

        // A deletion timestamp confirms that the server accepted the delete.
        host.update(cx, |host, _| {
            host.converge_pending(
                "uid-1",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: terminating("uid-1"),
                },
            );
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 1);

        // A delete event also confirms the operation.
        host.update(cx, |host, _| {
            host.converge_pending(
                "uid-2",
                &StoreEvent {
                    op: StoreOp::Delete,
                    obj: object("uid-2"),
                },
            );
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 0);
    }

    #[gpui_kit::test]
    fn resolve_pending_rolls_back_failed_ops(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.mark_pending("uid-1", PendingOp::Restart);
            assert!(host.pending("uid-1").is_some());
            assert_eq!(host.resolve_pending("uid-1"), Some(PendingOp::Restart));
            assert_eq!(host.resolve_pending("uid-1"), None);
        });
    }

    fn scaled_object(uid: &str, resource_version: &str, replicas: i64) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "metadata": { "uid": uid, "name": uid, "resourceVersion": resource_version },
                "spec": { "replicas": replicas }
            }))
            .expect("synthetic object"),
        )
    }

    fn terminating(uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "metadata": {
                    "uid": uid,
                    "name": uid,
                    "deletionTimestamp": "2026-09-23T10:00:00Z",
                }
            }))
            .expect("synthetic object"),
        )
    }

    // A source that cannot start yet reports the startup placeholder. The
    // table must show the loading skeleton, not a failure.
    #[gpui_kit::test]
    fn startup_loading_reason_resolves_to_listing(cx: &mut TestAppContext) {
        let factory = reason_factory(crate::shell::STARTUP_LOADING_REASON);
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);
        host.update(cx, |host, _cx| {
            assert!(host.is_startup_loading());
            assert_eq!(host.status(), TableStatus::Listing);
            assert!(host.snapshot().is_none(), "no rows during startup");
        });
    }

    // A real failure still reaches the error state.
    #[gpui_kit::test]
    fn watch_failure_still_reports_failed(cx: &mut TestAppContext) {
        let host = cx.update(|cx| {
            cx.new(|cx| {
                TableHost::new(
                    &pod_columns(),
                    reason_factory("forbidden: cannot list pods"),
                    cx,
                )
            })
        });
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);
        host.update(cx, |host, _cx| {
            assert!(!host.is_startup_loading());
            assert_eq!(
                host.status(),
                TableStatus::Failed("forbidden: cannot list pods".to_owned())
            );
        });
    }

    fn reason_factory(reason: &'static str) -> SourceFactory {
        Box::new(move || Box::new(ReasonOnlySource::new(reason)) as Box<dyn ResourceSource>)
    }

    /// Mirrors `UnavailableSource`: one error and no rows.
    struct ReasonOnlySource {
        reason: &'static str,
    }

    impl ReasonOnlySource {
        fn new(reason: &'static str) -> Self {
            Self { reason }
        }

        fn error(&self) -> SourceEvent {
            SourceEvent::Error {
                reason: self.reason.to_owned(),
            }
        }
    }

    impl ResourceSource for ReasonOnlySource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(self.error());
            Box::new(CancelSubscription)
        }

        /// Delivers the error on the bounded channel the host reads. The
        /// default `subscribe_bounded` hands the legacy unbounded channel to a
        /// real thread, and `pump_source` only moves the simulated clock, so
        /// the error could still be in flight when the test asserts. Every
        /// other source in this file writes to the bounded channel directly.
        fn subscribe_bounded(
            &mut self,
            events: mpsc::Sender<SourceEvent>,
        ) -> Box<dyn Subscription> {
            let _ = events.try_send(self.error());
            Box::new(CancelSubscription)
        }
    }

    struct CancelSubscription;

    impl Subscription for CancelSubscription {
        fn cancel(&mut self) {}
    }

    #[gpui_kit::test]
    fn trailing_flush_rearm_keeps_original_deadline(cx: &mut TestAppContext) {
        let factory: SourceFactory = Box::new(|| {
            Box::new(FakeSource::new(
                FakeSourceConfig::default(),
                ChurnHandle::new(false),
            )) as Box<dyn ResourceSource>
        });
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        cx.executor().advance_clock(Duration::from_millis(100));
        host.update(cx, |host, cx| {
            host.merge.mark();
            host.on_flush_timer(cx);
        });
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        assert!(!cx.update(|cx| host.read(cx).merge.dirty));
    }

    // The trailing timer must apply the final burst after the merge window.
    #[gpui_kit::test]
    fn trailing_event_batch_converges_without_further_events(cx: &mut TestAppContext) {
        use std::sync::Mutex;
        use tokio::sync::mpsc;

        struct ChannelSource {
            sender: Arc<Mutex<Option<mpsc::Sender<SourceEvent>>>>,
        }

        struct NoopSubscription;

        impl Subscription for NoopSubscription {
            fn cancel(&mut self) {}
        }

        impl ResourceSource for ChannelSource {
            fn subscribe(
                &mut self,
                _events: tokio::sync::mpsc::UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(NoopSubscription)
            }

            fn subscribe_bounded(
                &mut self,
                events: mpsc::Sender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                *self.sender.lock().expect("sender slot") = Some(events);
                Box::new(NoopSubscription)
            }
        }

        let sender_slot = Arc::new(Mutex::new(None));
        let factory: SourceFactory = {
            let slot = Arc::clone(&sender_slot);
            Box::new(move || {
                Box::new(ChannelSource {
                    sender: Arc::clone(&slot),
                }) as Box<dyn ResourceSource>
            })
        };
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);

        let sender = sender_slot
            .lock()
            .expect("sender slot")
            .clone()
            .expect("SpawnWatch must subscribe to the source");
        sender.try_send(SourceEvent::Init).expect("init");
        for index in 0..3 {
            sender
                .try_send(SourceEvent::Store(apply(&format!("uid-{index}"))))
                .expect("store");
        }
        sender.try_send(SourceEvent::InitDone).expect("init done");
        pump_source(cx);
        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 3);

        // No more events follow the delete.
        sender
            .try_send(SourceEvent::Store(StoreEvent {
                op: StoreOp::Delete,
                obj: object("uid-0"),
            }))
            .expect("delete");
        pump_source(cx);
        assert_eq!(
            cx.update(|cx| host.read(cx).row_count()),
            3,
            "No rebuild before the merge window closes"
        );

        cx.executor()
            .advance_clock(MERGE_INTERVAL + Duration::from_millis(1));
        pump_source(cx);
        assert_eq!(
            cx.update(|cx| host.read(cx).row_count()),
            2,
            "Trailing events must converge after the fallback flush"
        );
    }

    #[gpui_kit::test]
    fn watch_error_after_init_keeps_rows_but_marks_them_stale(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, cx| {
            host.dispatch(TableEvent::Start, cx);
            host.on_source_event(SourceEvent::Init, cx);
            host.on_source_event(SourceEvent::Store(apply("uid-1")), cx);
            host.on_source_event(SourceEvent::InitDone, cx);
            host.dispatch(
                TableEvent::SnapshotUpdated {
                    snapshot: Arc::new(table_snapshot(&[("pod-1", "uid-1")], "", None)),
                    generation: 1,
                },
                cx,
            );
            host.on_source_event(
                SourceEvent::Error {
                    reason: "watch disconnected".to_owned(),
                },
                cx,
            );
        });

        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 1);
        assert_eq!(
            cx.update(|cx| host.read(cx).status()),
            TableStatus::Stale("watch disconnected".to_owned())
        );
    }

    #[gpui_kit::test]
    fn disconnected_watch_after_init_does_not_stay_live(cx: &mut TestAppContext) {
        struct ChannelSource {
            sender: Arc<std::sync::Mutex<Option<mpsc::Sender<SourceEvent>>>>,
        }
        struct NoopSubscription;
        impl Subscription for NoopSubscription {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for ChannelSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(NoopSubscription)
            }

            fn subscribe_bounded(
                &mut self,
                events: mpsc::Sender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                *self.sender.lock().expect("sender slot") = Some(events);
                Box::new(NoopSubscription)
            }
        }

        let sender_slot = Arc::new(std::sync::Mutex::new(None));
        let factory: SourceFactory = {
            let slot = Arc::clone(&sender_slot);
            Box::new(move || {
                Box::new(ChannelSource {
                    sender: Arc::clone(&slot),
                }) as Box<dyn ResourceSource>
            })
        };
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);
        let sender = sender_slot
            .lock()
            .expect("sender slot")
            .take()
            .expect("SpawnWatch must subscribe");
        sender.try_send(SourceEvent::Init).expect("init");
        sender
            .try_send(SourceEvent::Store(apply("uid-1")))
            .expect("store");
        sender.try_send(SourceEvent::InitDone).expect("init done");
        pump_source(cx);
        cx.run_until_parked();
        drop(sender);
        pump_source(cx);
        pump_source(cx);

        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 1);
        assert!(matches!(
            cx.update(|cx| host.read(cx).status()),
            TableStatus::Stale(_)
        ));
    }

    #[gpui_kit::test]
    fn rebuild_requests_keep_one_in_flight_and_latest_pending(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, cx| {
            host.store.apply(&apply("uid-1"));
            host.rebuild(Filter::default(), None, 1, cx);
            host.rebuild(Filter::default(), None, 2, cx);
            host.rebuild(Filter::default(), None, 3, cx);
            assert!(host.active_rebuild.is_some());
            assert_eq!(
                host.pending_rebuild
                    .as_ref()
                    .and_then(|request| request.generation),
                Some(3)
            );
            let rebuild_id = host.active_rebuild.expect("active rebuild");
            let watch_epoch = host.epoch.get();
            host.epoch.set(watch_epoch.wrapping_add(1));
            host.finish_rebuild(
                rebuild_id,
                watch_epoch,
                false,
                None,
                table_snapshot(&[("old", "uid-old")], "", None),
                cx,
            );
            assert!(host.display_snapshot.is_none());
            assert!(host.pending_rebuild.is_none());
        });
    }

    #[gpui_kit::test]
    fn pending_updates_require_the_requested_target_fields(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: Arc::new(
                    serde_json::from_value(json!({
                        "metadata": { "uid": "scale", "resourceVersion": "10" },
                        "spec": { "replicas": 1 }
                    }))
                    .expect("scale object"),
                ),
            });
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: Arc::new(
                    serde_json::from_value(json!({
                        "metadata": { "uid": "restart", "resourceVersion": "20" },
                        "spec": { "template": { "metadata": { "annotations": {
                            "kubectl.kubernetes.io/restartedAt": "old"
                        } } } }
                    }))
                    .expect("restart object"),
                ),
            });
            assert_eq!(
                host.store
                    .get("restart")
                    .and_then(|obj| obj.data.pointer(
                        "/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt"
                    ))
                    .and_then(|value| value.as_str()),
                Some("old")
            );
            host.mark_pending("scale", PendingOp::Scale { replicas: 3 });
            host.mark_pending("restart", PendingOp::Restart);
            let restart = host.pending.get("restart").expect("restart pending");
            assert_eq!(restart.resource_version.as_deref(), Some("20"));
            assert_eq!(restart.previous_restart_annotation.as_deref(), Some("old"));
        });

        host.update(cx, |host, _| {
            host.converge_pending(
                "scale",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: Arc::new(
                        serde_json::from_value(json!({
                            "metadata": { "uid": "scale", "resourceVersion": "11" },
                            "spec": { "replicas": 1 }
                        }))
                        .expect("scale update"),
                    ),
                },
            );
            host.converge_pending(
                "restart",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: Arc::new(
                        serde_json::from_value(json!({
                            "metadata": { "uid": "restart", "resourceVersion": "21" },
                            "spec": { "template": { "metadata": { "annotations": {
                                "kubectl.kubernetes.io/restartedAt": "old"
                            } } } }
                        }))
                        .expect("restart update"),
                    ),
                },
            );
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 2);

        host.update(cx, |host, _| {
            host.converge_pending(
                "scale",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: Arc::new(
                        serde_json::from_value(json!({
                            "metadata": { "uid": "scale", "resourceVersion": "12" },
                            "spec": { "replicas": 3 }
                        }))
                        .expect("scale confirmation"),
                    ),
                },
            );
            host.converge_pending(
                "restart",
                &StoreEvent {
                    op: StoreOp::Apply,
                    obj: Arc::new(
                        serde_json::from_value(json!({
                            "metadata": { "uid": "restart", "resourceVersion": "22" },
                            "spec": { "template": { "metadata": { "annotations": {
                                "kubectl.kubernetes.io/restartedAt": "new"
                            } } } }
                        }))
                        .expect("restart confirmation"),
                    ),
                },
            );
        });
        assert_eq!(cx.update(|cx| host.read(cx).pending_count()), 0);
    }

    #[gpui_kit::test]
    fn cache_primed_marks_rows_until_live_init_done(cx: &mut TestAppContext) {
        use std::sync::Mutex;
        use tokio::sync::mpsc;

        struct ChannelSource {
            sender: Arc<Mutex<Option<mpsc::Sender<SourceEvent>>>>,
        }

        struct NoopSubscription;

        impl Subscription for NoopSubscription {
            fn cancel(&mut self) {}
        }

        impl ResourceSource for ChannelSource {
            fn subscribe(
                &mut self,
                _events: tokio::sync::mpsc::UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(NoopSubscription)
            }

            fn subscribe_bounded(
                &mut self,
                events: mpsc::Sender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                *self.sender.lock().expect("sender slot") = Some(events);
                Box::new(NoopSubscription)
            }
        }

        let sender_slot = Arc::new(Mutex::new(None));
        let factory: SourceFactory = {
            let slot = Arc::clone(&sender_slot);
            Box::new(move || {
                Box::new(ChannelSource {
                    sender: Arc::clone(&slot),
                }) as Box<dyn ResourceSource>
            })
        };
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);

        let sender = sender_slot
            .lock()
            .expect("sender slot")
            .clone()
            .expect("SpawnWatch must subscribe to the source");
        sender.try_send(SourceEvent::Init).expect("init");
        sender
            .try_send(SourceEvent::Store(apply("uid-cached")))
            .expect("cached row");
        sender
            .try_send(SourceEvent::CachePrimed {
                stale: true,
                saved_at: std::time::SystemTime::now(),
            })
            .expect("cache primed");
        pump_source(cx);
        pump_source(cx);
        host.read_with(cx, |host, _| {
            assert_eq!(host.row_count(), 1);
            let cached = host
                .cached()
                .expect("Cache marker must appear before live InitDone");
            assert!(cached.stale);
            assert_eq!(host.status(), TableStatus::Listing);
            assert_eq!(
                host.first_response(),
                None,
                "Cache hits do not count as initial response latency"
            );
        });

        sender.try_send(SourceEvent::Init).expect("live init");
        sender
            .try_send(SourceEvent::Store(apply("uid-live")))
            .expect("live row");
        sender.try_send(SourceEvent::InitDone).expect("init done");
        pump_source(cx);
        pump_source(cx);
        host.read_with(cx, |host, _| {
            assert_eq!(host.row_count(), 1);
            assert!(
                host.cached().is_none(),
                "The cache marker clears after live sync"
            );
            assert_eq!(host.status(), TableStatus::Streaming);
            assert!(
                host.first_response().is_some(),
                "Live response timing restarts"
            );
        });
    }

    // Cached rows stay readable when the live list never arrives.
    #[gpui_kit::test]
    fn cached_rows_outrank_a_failed_initial_list(cx: &mut TestAppContext) {
        use std::sync::Mutex;
        use tokio::sync::mpsc;

        struct ChannelSource {
            sender: Arc<Mutex<Option<mpsc::Sender<SourceEvent>>>>,
        }

        struct NoopSubscription;

        impl Subscription for NoopSubscription {
            fn cancel(&mut self) {}
        }

        impl ResourceSource for ChannelSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(NoopSubscription)
            }

            fn subscribe_bounded(
                &mut self,
                events: mpsc::Sender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                *self.sender.lock().expect("sender slot") = Some(events);
                Box::new(NoopSubscription)
            }
        }

        let sender_slot = Arc::new(Mutex::new(None));
        let factory: SourceFactory = {
            let slot = Arc::clone(&sender_slot);
            Box::new(move || {
                Box::new(ChannelSource {
                    sender: Arc::clone(&slot),
                }) as Box<dyn ResourceSource>
            })
        };
        let host = cx.update(|cx| cx.new(|cx| TableHost::new(&pod_columns(), factory, cx)));
        host.update(cx, |host, cx| host.dispatch(TableEvent::Start, cx));
        pump_source(cx);

        let sender = sender_slot
            .lock()
            .expect("sender slot")
            .clone()
            .expect("SpawnWatch must subscribe to the source");
        sender.try_send(SourceEvent::Init).expect("init");
        sender
            .try_send(SourceEvent::Store(apply("uid-cached")))
            .expect("cached row");
        sender
            .try_send(SourceEvent::CachePrimed {
                stale: true,
                saved_at: SystemTime::now(),
            })
            .expect("cache primed");
        pump_source(cx);
        pump_source(cx);
        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 1);

        sender
            .try_send(SourceEvent::Error {
                reason: "the API server refused the list".to_owned(),
            })
            .expect("error");
        pump_source(cx);

        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 1);
        assert_eq!(
            cx.update(|cx| host.read(cx).status()),
            TableStatus::Stale("the API server refused the list".to_owned()),
            "Cached rows keep the table readable instead of failing over"
        );
        assert!(
            cx.update(|cx| host.read(cx).cached()).is_some(),
            "The cache marker still explains where the rows come from"
        );

        // Retry reconnects and drops the cached rows for the new attempt.
        host.update(cx, |host, cx| host.refresh(cx));
        assert_eq!(cx.update(|cx| host.read(cx).row_count()), 0);
        assert_eq!(cx.update(|cx| host.read(cx).status()), TableStatus::Listing);
    }

    // A failure before any row keeps the error state and its Retry path.
    #[gpui_kit::test]
    fn a_failed_list_without_rows_stays_failed(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, cx| {
            host.dispatch(TableEvent::Start, cx);
            host.on_source_event(
                SourceEvent::Error {
                    reason: "no kubeconfig".to_owned(),
                },
                cx,
            );
        });
        assert_eq!(host.read_with(cx, |host, _| host.row_count()), 0);
        assert_eq!(
            host.read_with(cx, |host, _| host.status()),
            TableStatus::Failed("no kubeconfig".to_owned())
        );
    }

    #[gpui_kit::test]
    fn pending_labels_name_the_resource_or_fall_back_to_the_uid(cx: &mut TestAppContext) {
        struct NullSource;
        struct Noop;
        impl Subscription for Noop {
            fn cancel(&mut self) {}
        }
        impl ResourceSource for NullSource {
            fn subscribe(
                &mut self,
                _events: UnboundedSender<SourceEvent>,
            ) -> Box<dyn Subscription> {
                Box::new(Noop)
            }
        }

        let host = cx.update(|cx| {
            let factory: SourceFactory =
                Box::new(|| Box::new(NullSource) as Box<dyn ResourceSource>);
            cx.new(|cx| TableHost::new(&pod_columns(), factory, cx))
        });
        host.update(cx, |host, _| {
            host.store.apply(&StoreEvent {
                op: StoreOp::Apply,
                obj: named_object("web-0", "uid-1"),
            });
            assert_eq!(
                host.pending_label("uid-1", &PendingOp::Delete),
                "Delete web-0"
            );
            assert_eq!(
                host.pending_label("uid-1", &PendingOp::Scale { replicas: 2 }),
                "Scale web-0"
            );
            assert_eq!(
                host.pending_label("uid-missing", &PendingOp::Restart),
                "Restart (uid-missing)"
            );
        });
    }
}
