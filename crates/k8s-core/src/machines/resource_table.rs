use std::sync::Arc;

use statig::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::projection::{Column, Filter, IndexSnapshot, Sort};

#[derive(Clone, Debug)]
pub enum TableEvent {
    /// User opens the view.
    Start,
    /// The reflector initial list ended.
    WatchInitDone,
    /// A background snapshot rebuild completed.
    SnapshotUpdated {
        snapshot: Arc<IndexSnapshot>,
        generation: u64,
    },
    /// The watch or store failed.
    StoreError {
        reason: String,
    },
    /// Freeze live updates and keep the last snapshot.
    Pause,
    /// Resume updates and rebuild when needed.
    Resume,
    FilterChanged {
        filter: Filter,
    },
    SortChanged {
        sort: Option<Sort>,
    },
    /// Restart the watch after an error.
    Retry,
    /// Close the view.
    Stop,
}

/// Effects executed by the UI or Tokio layer.
#[derive(Clone, Debug)]
pub enum TableEffect {
    /// Start the watcher and reflector.
    SpawnWatch,
    /// Cancel the watch by dropping the controller.
    CancelWatch,
    /// Rebuild the snapshot in the background.
    RebuildSnapshot {
        filter: Filter,
        sort: Option<Sort>,
        generation: u64,
    },
    /// Notify the UI after a state change.
    Notify,
}

/// Shared state contains effects, projection inputs, and the latest snapshot.
pub struct ResourceTableMachine {
    effects: UnboundedSender<TableEffect>,
    filter: Filter,
    sort: Option<Sort>,
    snapshot: Option<Arc<IndexSnapshot>>,
    /// Highest issued rebuild generation.
    generation: u64,
    /// Highest applied snapshot generation.
    applied_generation: u64,
    /// Store updates were dropped while paused.
    rebuild_pending: bool,
}

impl ResourceTableMachine {
    pub fn new(effects: UnboundedSender<TableEffect>, _columns: Vec<Column>) -> Self {
        Self {
            effects,
            filter: Filter::default(),
            sort: None,
            snapshot: None,
            generation: 0,
            applied_generation: 0,
            rebuild_pending: false,
        }
    }

    pub fn snapshot(&self) -> Option<&Arc<IndexSnapshot>> {
        self.snapshot.as_ref()
    }

    pub fn sort(&self) -> Option<Sort> {
        self.sort
    }

    fn emit(&self, effect: TableEffect) {
        let _ = self.effects.send(effect);
    }

    fn request_rebuild(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.emit(TableEffect::RebuildSnapshot {
            filter: self.filter.clone(),
            sort: self.sort,
            generation: self.generation,
        });
    }

    fn accept_snapshot(&mut self, snapshot: &Arc<IndexSnapshot>, generation: u64) -> bool {
        if generation < self.applied_generation {
            return false;
        }
        self.snapshot = Some(Arc::clone(snapshot));
        self.applied_generation = generation;
        true
    }
}

#[state_machine(
    initial = "State::idle()",
    state(derive(Debug)),
    superstate(derive(Debug))
)]
impl ResourceTableMachine {
    #[state]
    fn idle(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::Start => {
                self.emit(TableEffect::SpawnWatch);
                self.emit(TableEffect::Notify);
                Transition(State::listing())
            }
            TableEvent::FilterChanged { filter } => {
                self.filter = filter.clone();
                Handled
            }
            TableEvent::SortChanged { sort } => {
                self.sort = *sort;
                Handled
            }
            TableEvent::Stop => {
                self.emit(TableEffect::CancelWatch);
                self.emit(TableEffect::Notify);
                Transition(State::stopped())
            }
            _ => Handled,
        }
    }

    #[state(superstate = "active")]
    fn listing(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::WatchInitDone => {
                self.request_rebuild();
                Handled
            }
            TableEvent::SnapshotUpdated {
                snapshot,
                generation,
            } => {
                if self.accept_snapshot(snapshot, *generation) {
                    self.emit(TableEffect::Notify);
                    Transition(State::streaming())
                } else {
                    Handled
                }
            }
            _ => Super,
        }
    }

    #[state(superstate = "active")]
    fn streaming(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::WatchInitDone => {
                self.request_rebuild();
                Handled
            }
            TableEvent::SnapshotUpdated {
                snapshot,
                generation,
            } => {
                if self.accept_snapshot(snapshot, *generation) {
                    self.emit(TableEffect::Notify);
                }
                Handled
            }
            _ => Super,
        }
    }

    /// Drop store updates while paused and rebuild once on resume.
    #[state(superstate = "active")]
    fn paused(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::Pause => Handled,
            TableEvent::Resume => {
                let was_pending = self.rebuild_pending;
                self.rebuild_pending = false;
                if was_pending {
                    self.request_rebuild();
                }
                self.emit(TableEffect::Notify);
                if self.snapshot.is_some() {
                    Transition(State::streaming())
                } else {
                    Transition(State::listing())
                }
            }
            TableEvent::SnapshotUpdated { .. } | TableEvent::WatchInitDone => {
                self.rebuild_pending = true;
                Handled
            }
            TableEvent::FilterChanged { filter } => {
                self.filter = filter.clone();
                self.rebuild_pending = true;
                Handled
            }
            TableEvent::SortChanged { sort } => {
                self.sort = *sort;
                self.rebuild_pending = true;
                Handled
            }
            _ => Super,
        }
    }

    #[superstate]
    fn active(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::Stop => {
                self.emit(TableEffect::CancelWatch);
                self.emit(TableEffect::Notify);
                Transition(State::stopped())
            }
            TableEvent::StoreError { reason } => {
                self.emit(TableEffect::CancelWatch);
                self.emit(TableEffect::Notify);
                Transition(State::failed(reason.clone()))
            }
            TableEvent::Pause => {
                self.emit(TableEffect::Notify);
                Transition(State::paused())
            }
            TableEvent::FilterChanged { filter } => {
                self.filter = filter.clone();
                self.request_rebuild();
                Handled
            }
            TableEvent::SortChanged { sort } => {
                self.sort = *sort;
                self.request_rebuild();
                Handled
            }
            _ => Handled,
        }
    }

    #[state(local_storage("reason: String"))]
    fn failed(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::Retry => {
                self.emit(TableEffect::SpawnWatch);
                self.emit(TableEffect::Notify);
                Transition(State::listing())
            }
            TableEvent::Stop => {
                self.emit(TableEffect::CancelWatch);
                self.emit(TableEffect::Notify);
                Transition(State::stopped())
            }
            TableEvent::FilterChanged { filter } => {
                self.filter = filter.clone();
                Handled
            }
            TableEvent::SortChanged { sort } => {
                self.sort = *sort;
                Handled
            }
            _ => Handled,
        }
    }

    #[state]
    fn stopped(&mut self, event: &TableEvent) -> Outcome<State> {
        match event {
            TableEvent::Start => {
                self.emit(TableEffect::SpawnWatch);
                self.emit(TableEffect::Notify);
                Transition(State::listing())
            }
            _ => Handled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use statig::blocking::StateMachine;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ExpectedState {
        Idle,
        Listing,
        Streaming,
        Paused,
        Failed(String),
        Stopped,
    }

    fn state_spec(state: &State) -> ExpectedState {
        match state {
            State::Idle {} => ExpectedState::Idle,
            State::Listing {} => ExpectedState::Listing,
            State::Streaming {} => ExpectedState::Streaming,
            State::Paused {} => ExpectedState::Paused,
            State::Failed { reason } => ExpectedState::Failed(reason.clone()),
            State::Stopped {} => ExpectedState::Stopped,
        }
    }

    /// `Filter` and `Selector` do not implement `PartialEq`, so effects compare selected fields.
    #[derive(Clone, Debug, PartialEq)]
    enum EffectSpec {
        SpawnWatch,
        CancelWatch,
        Notify,
        Rebuild {
            namespace: Option<String>,
            sort: Option<Sort>,
            generation: u64,
        },
    }

    fn effect_spec(effect: &TableEffect) -> EffectSpec {
        match effect {
            TableEffect::SpawnWatch => EffectSpec::SpawnWatch,
            TableEffect::CancelWatch => EffectSpec::CancelWatch,
            TableEffect::Notify => EffectSpec::Notify,
            TableEffect::RebuildSnapshot {
                filter,
                sort,
                generation,
            } => EffectSpec::Rebuild {
                namespace: filter.namespace.clone(),
                sort: *sort,
                generation: *generation,
            },
        }
    }

    fn filter_ns(namespace: &str) -> Filter {
        Filter {
            namespace: Some(namespace.to_string()),
            ..Filter::default()
        }
    }

    fn snapshot(generation: u64) -> Arc<IndexSnapshot> {
        Arc::new(IndexSnapshot {
            generation,
            ..IndexSnapshot::default()
        })
    }

    struct Harness {
        machine: StateMachine<ResourceTableMachine>,
        effects: UnboundedReceiver<TableEffect>,
    }

    impl Harness {
        fn new() -> Self {
            let (effects_tx, effects_rx) = unbounded_channel();
            Self {
                machine: ResourceTableMachine::new(effects_tx, Vec::new()).state_machine(),
                effects: effects_rx,
            }
        }

        fn send(&mut self, event: &TableEvent) -> Vec<EffectSpec> {
            self.machine.handle(event);
            let mut collected = Vec::new();
            while let Ok(effect) = self.effects.try_recv() {
                collected.push(effect_spec(&effect));
            }
            collected
        }

        fn state(&self) -> ExpectedState {
            state_spec(self.machine.state())
        }

        /// Follow valid transitions to reach the target state.
        fn reach(&mut self, target: &ExpectedState) {
            match target {
                ExpectedState::Idle => {}
                ExpectedState::Listing => {
                    self.send(&TableEvent::Start);
                }
                ExpectedState::Streaming | ExpectedState::Paused => {
                    self.send(&TableEvent::Start);
                    self.send(&TableEvent::WatchInitDone);
                    self.send(&TableEvent::SnapshotUpdated {
                        snapshot: snapshot(1),
                        generation: 1,
                    });
                    if *target == ExpectedState::Paused {
                        self.send(&TableEvent::Pause);
                    }
                }
                ExpectedState::Failed(reason) => {
                    self.send(&TableEvent::Start);
                    self.send(&TableEvent::StoreError {
                        reason: reason.clone(),
                    });
                }
                ExpectedState::Stopped => {
                    self.send(&TableEvent::Stop);
                }
            }
            assert_eq!(self.state(), target.clone());
        }
    }

    type Row = (TableEvent, ExpectedState, Vec<EffectSpec>);

    /// Expected results for every state and event pair.
    fn cases(state: &ExpectedState) -> Vec<Row> {
        use EffectSpec::{CancelWatch, Notify, Rebuild, SpawnWatch};
        let filter = || filter_ns("prod");
        let sort = || Some(Sort::ascending(0));
        let update = || TableEvent::SnapshotUpdated {
            snapshot: snapshot(1),
            generation: 1,
        };
        let error = || TableEvent::StoreError {
            reason: "boom".to_string(),
        };
        let filter_changed = || TableEvent::FilterChanged { filter: filter() };
        let sort_changed = || TableEvent::SortChanged { sort: sort() };

        match state {
            // Before Start, filter and sort only update storage.
            ExpectedState::Idle => vec![
                (
                    TableEvent::Start,
                    ExpectedState::Listing,
                    vec![SpawnWatch, Notify],
                ),
                (TableEvent::WatchInitDone, ExpectedState::Idle, vec![]),
                (update(), ExpectedState::Idle, vec![]),
                (error(), ExpectedState::Idle, vec![]),
                (TableEvent::Pause, ExpectedState::Idle, vec![]),
                (TableEvent::Resume, ExpectedState::Idle, vec![]),
                (filter_changed(), ExpectedState::Idle, vec![]),
                (sort_changed(), ExpectedState::Idle, vec![]),
                (TableEvent::Retry, ExpectedState::Idle, vec![]),
                (
                    TableEvent::Stop,
                    ExpectedState::Stopped,
                    vec![CancelWatch, Notify],
                ),
            ],
            // generation is zero after reaching this state
            ExpectedState::Listing => vec![
                (TableEvent::Start, ExpectedState::Listing, vec![]),
                (
                    TableEvent::WatchInitDone,
                    ExpectedState::Listing,
                    vec![Rebuild {
                        namespace: None,
                        sort: None,
                        generation: 1,
                    }],
                ),
                (update(), ExpectedState::Streaming, vec![Notify]),
                (
                    error(),
                    ExpectedState::Failed("boom".to_string()),
                    vec![CancelWatch, Notify],
                ),
                (TableEvent::Pause, ExpectedState::Paused, vec![Notify]),
                (TableEvent::Resume, ExpectedState::Listing, vec![]),
                (
                    filter_changed(),
                    ExpectedState::Listing,
                    vec![Rebuild {
                        namespace: Some("prod".to_string()),
                        sort: None,
                        generation: 1,
                    }],
                ),
                (
                    sort_changed(),
                    ExpectedState::Listing,
                    vec![Rebuild {
                        namespace: None,
                        sort: sort(),
                        generation: 1,
                    }],
                ),
                (TableEvent::Retry, ExpectedState::Listing, vec![]),
                (
                    TableEvent::Stop,
                    ExpectedState::Stopped,
                    vec![CancelWatch, Notify],
                ),
            ],
            // generation is one after reaching this state
            ExpectedState::Streaming => vec![
                (TableEvent::Start, ExpectedState::Streaming, vec![]),
                (
                    TableEvent::WatchInitDone,
                    ExpectedState::Streaming,
                    vec![Rebuild {
                        namespace: None,
                        sort: None,
                        generation: 2,
                    }],
                ),
                (update(), ExpectedState::Streaming, vec![Notify]),
                (
                    error(),
                    ExpectedState::Failed("boom".to_string()),
                    vec![CancelWatch, Notify],
                ),
                (TableEvent::Pause, ExpectedState::Paused, vec![Notify]),
                (TableEvent::Resume, ExpectedState::Streaming, vec![]),
                (
                    filter_changed(),
                    ExpectedState::Streaming,
                    vec![Rebuild {
                        namespace: Some("prod".to_string()),
                        sort: None,
                        generation: 2,
                    }],
                ),
                (
                    sort_changed(),
                    ExpectedState::Streaming,
                    vec![Rebuild {
                        namespace: None,
                        sort: sort(),
                        generation: 2,
                    }],
                ),
                (TableEvent::Retry, ExpectedState::Streaming, vec![]),
                (
                    TableEvent::Stop,
                    ExpectedState::Stopped,
                    vec![CancelWatch, Notify],
                ),
            ],
            // While paused, keep state and record pending updates.
            ExpectedState::Paused => vec![
                (TableEvent::Start, ExpectedState::Paused, vec![]),
                (TableEvent::WatchInitDone, ExpectedState::Paused, vec![]),
                (update(), ExpectedState::Paused, vec![]),
                (
                    error(),
                    ExpectedState::Failed("boom".to_string()),
                    vec![CancelWatch, Notify],
                ),
                (TableEvent::Pause, ExpectedState::Paused, vec![]),
                (TableEvent::Resume, ExpectedState::Streaming, vec![Notify]),
                (filter_changed(), ExpectedState::Paused, vec![]),
                (sort_changed(), ExpectedState::Paused, vec![]),
                (TableEvent::Retry, ExpectedState::Paused, vec![]),
                (
                    TableEvent::Stop,
                    ExpectedState::Stopped,
                    vec![CancelWatch, Notify],
                ),
            ],
            ExpectedState::Failed(_) => vec![
                (
                    TableEvent::Start,
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (
                    TableEvent::WatchInitDone,
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (update(), ExpectedState::Failed("boom".to_string()), vec![]),
                (error(), ExpectedState::Failed("boom".to_string()), vec![]),
                (
                    TableEvent::Pause,
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (
                    TableEvent::Resume,
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (
                    filter_changed(),
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (
                    sort_changed(),
                    ExpectedState::Failed("boom".to_string()),
                    vec![],
                ),
                (
                    TableEvent::Retry,
                    ExpectedState::Listing,
                    vec![SpawnWatch, Notify],
                ),
                (
                    TableEvent::Stop,
                    ExpectedState::Stopped,
                    vec![CancelWatch, Notify],
                ),
            ],
            ExpectedState::Stopped => vec![
                (
                    TableEvent::Start,
                    ExpectedState::Listing,
                    vec![SpawnWatch, Notify],
                ),
                (TableEvent::WatchInitDone, ExpectedState::Stopped, vec![]),
                (update(), ExpectedState::Stopped, vec![]),
                (error(), ExpectedState::Stopped, vec![]),
                (TableEvent::Pause, ExpectedState::Stopped, vec![]),
                (TableEvent::Resume, ExpectedState::Stopped, vec![]),
                (filter_changed(), ExpectedState::Stopped, vec![]),
                (sort_changed(), ExpectedState::Stopped, vec![]),
                (TableEvent::Retry, ExpectedState::Stopped, vec![]),
                (TableEvent::Stop, ExpectedState::Stopped, vec![]),
            ],
        }
    }

    #[test]
    fn transition_matrix_covers_every_state_event_pair() {
        let states = [
            ExpectedState::Idle,
            ExpectedState::Listing,
            ExpectedState::Streaming,
            ExpectedState::Paused,
            ExpectedState::Failed("boom".to_string()),
            ExpectedState::Stopped,
        ];

        let mut checked = 0;
        for state in &states {
            let mut probe = Harness::new();
            probe.reach(state);
            let rows = cases(state);
            assert_eq!(rows.len(), 10, "every event has one row: {state:?}");

            for (event, expected_state, expected_effects) in rows {
                let mut harness = Harness::new();
                harness.reach(state);
                let effects = harness.send(&event);
                assert_eq!(harness.state(), expected_state, "{event:?} in {state:?}");
                assert_eq!(effects, expected_effects, "{event:?} in {state:?}");
                checked += 1;
            }
        }
        assert_eq!(checked, states.len() * 10);
    }

    #[test]
    fn paused_discards_updates_and_resume_rebuilds_with_current_inputs() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Paused);

        // Paused updates are dropped and recorded as pending.
        assert!(
            harness
                .send(&TableEvent::SnapshotUpdated {
                    snapshot: snapshot(2),
                    generation: 2,
                })
                .is_empty()
        );
        assert!(
            harness
                .send(&TableEvent::FilterChanged {
                    filter: filter_ns("prod")
                })
                .is_empty()
        );
        assert_eq!(harness.state(), ExpectedState::Paused);

        // Resume rebuilds with the current filter and clears pending.
        let effects = harness.send(&TableEvent::Resume);
        assert_eq!(
            effects,
            vec![
                EffectSpec::Rebuild {
                    namespace: Some("prod".to_string()),
                    sort: None,
                    generation: 2,
                },
                EffectSpec::Notify,
            ]
        );
        assert_eq!(harness.state(), ExpectedState::Streaming);
        assert!(
            harness.send(&TableEvent::Resume).is_empty(),
            "Resume without pending does not rebuild"
        );
    }

    #[test]
    fn resume_without_snapshot_returns_to_listing() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Listing);
        harness.send(&TableEvent::Pause);
        assert_eq!(harness.state(), ExpectedState::Paused);

        let effects = harness.send(&TableEvent::Resume);
        assert_eq!(effects, vec![EffectSpec::Notify]);
        assert_eq!(harness.state(), ExpectedState::Listing);
    }

    #[test]
    fn stale_snapshot_generation_is_dropped() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Streaming);

        let effects = harness.send(&TableEvent::SnapshotUpdated {
            snapshot: snapshot(0),
            generation: 0,
        });
        assert!(effects.is_empty());
        assert_eq!(harness.state(), ExpectedState::Streaming);
        assert_eq!(
            harness.machine.inner().snapshot().map(|s| s.generation),
            Some(1),
            "an old generation cannot replace the applied snapshot"
        );
    }

    fn arb_event() -> impl Strategy<Value = TableEvent> {
        prop_oneof![
            Just(TableEvent::Start),
            Just(TableEvent::WatchInitDone),
            (0u64..8).prop_map(|generation| TableEvent::SnapshotUpdated {
                snapshot: snapshot(generation),
                generation,
            }),
            "[a-z]{0,8}".prop_map(|reason| TableEvent::StoreError { reason }),
            Just(TableEvent::Pause),
            Just(TableEvent::Resume),
            "[a-z]{0,8}".prop_map(|namespace| TableEvent::FilterChanged {
                filter: filter_ns(&namespace),
            }),
            prop_oneof![
                Just(None),
                Just(Some(Sort::ascending(0))),
                Just(Some(Sort::descending(1))),
            ]
            .prop_map(|sort| TableEvent::SortChanged { sort }),
            Just(TableEvent::Retry),
            Just(TableEvent::Stop),
        ]
    }

    proptest! {
        #[test]
        fn random_sequences_keep_invariants(events in prop::collection::vec(arb_event(), 0..200)) {
            let mut harness = Harness::new();
            let mut previous = harness.state();
            let mut ever_stopped = false;
            let mut last_generation = 0;

            for event in &events {
                let effects = harness.send(event);
                let next = harness.state();

                if previous == ExpectedState::Paused && next == ExpectedState::Paused {
                    prop_assert!(effects.is_empty(), "paused state sends no effect: {effects:?}");
                }
                if previous == ExpectedState::Paused && next != ExpectedState::Paused {
                    prop_assert!(matches!(
                        event,
                        TableEvent::Resume | TableEvent::Stop | TableEvent::StoreError { .. }
                    ), "only three event types leave Paused");
                }
                if ever_stopped && next != ExpectedState::Stopped {
                    prop_assert!(
                        matches!(event, TableEvent::Start),
                        "only Start leaves Stopped"
                    );
                }
                ever_stopped = next == ExpectedState::Stopped;

                for effect in &effects {
                    if let EffectSpec::Rebuild { generation, .. } = effect {
                        prop_assert!(*generation > last_generation, "generation must increase");
                        last_generation = *generation;
                    }
                }
                if next == ExpectedState::Streaming {
                    prop_assert!(harness.machine.inner().snapshot().is_some());
                }
                previous = next;
            }
        }
    }
}
