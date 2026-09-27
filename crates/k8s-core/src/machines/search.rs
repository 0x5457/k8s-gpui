use std::sync::Arc;

use kube::core::DynamicObject;
use statig::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::discovery::ResourceEntry;

pub const SEARCH_DEBOUNCE_MS: u64 = 200;
pub const SEARCH_RESULT_LIMIT: usize = 50;
/// Recent queries offered when the field is empty, newest first.
pub const SEARCH_RECENT_LIMIT: usize = 5;

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub resource: ResourceEntry,
    pub object: Arc<DynamicObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchPhase {
    Closed,
    Cleared,
    Query,
    Results,
    Error,
}

#[derive(Clone, Debug)]
pub enum SearchEvent {
    Open,
    QueryChanged {
        query: String,
    },
    Select {
        index: usize,
    },
    DebounceElapsed {
        request_id: u64,
    },
    Results {
        request_id: u64,
        hits: Vec<SearchHit>,
        scanned: usize,
        truncated: bool,
        partial: bool,
    },
    Error {
        request_id: u64,
        reason: String,
    },
    MoveSelection {
        delta: isize,
    },
    /// The user acted on a result, so the query becomes a recent search.
    Committed {
        query: String,
    },
    /// Reuse a recent search. It runs through the query path, so a recent search
    /// is debounced, cancellable, and stale-guarded like a typed one.
    Recall {
        query: String,
    },
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchEffect {
    Cancel { request_id: u64 },
    Schedule { request_id: u64, delay_ms: u64 },
    Query { request_id: u64, query: String },
    Notify,
}

pub struct SearchMachine {
    effects: UnboundedSender<SearchEffect>,
    query: String,
    active_request: Option<u64>,
    request_started: bool,
    next_request: u64,
    hits: Vec<SearchHit>,
    selected: usize,
    scanned: usize,
    truncated: bool,
    partial: bool,
    recents: Vec<String>,
}

impl SearchMachine {
    pub fn new(effects: UnboundedSender<SearchEffect>) -> Self {
        Self {
            effects,
            query: String::new(),
            active_request: None,
            request_started: false,
            next_request: 0,
            hits: Vec::new(),
            selected: 0,
            scanned: 0,
            truncated: false,
            partial: false,
            recents: Vec::new(),
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    /// Recent queries, newest first. They survive opening and closing the panel.
    pub fn recents(&self) -> &[String] {
        &self.recents
    }

    pub fn scanned(&self) -> usize {
        self.scanned
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn partial(&self) -> bool {
        self.partial
    }

    pub fn selected_index(&self) -> Option<usize> {
        if self.hits.is_empty() {
            None
        } else {
            Some(self.selected.min(self.hits.len() - 1))
        }
    }

    pub fn selected_hit(&self) -> Option<SearchHit> {
        self.selected_index()
            .and_then(|index| self.hits.get(index))
            .cloned()
    }

    fn emit(&self, effect: SearchEffect) {
        let _ = self.effects.send(effect);
    }

    fn notify(&self) {
        self.emit(SearchEffect::Notify);
    }

    fn cancel_active(&mut self) {
        self.request_started = false;
        if let Some(request_id) = self.active_request.take() {
            self.emit(SearchEffect::Cancel { request_id });
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    fn open(&mut self) -> Outcome<State> {
        self.cancel_active();
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
        self.scanned = 0;
        self.truncated = false;
        self.partial = false;
        self.notify();
        Transition(State::cleared())
    }

    fn replace_query(&mut self, query: String) -> Outcome<State> {
        self.cancel_active();
        self.query = query.trim().to_owned();
        self.hits.clear();
        self.selected = 0;
        self.scanned = 0;
        self.truncated = false;
        self.partial = false;
        if self.query.is_empty() {
            self.notify();
            return Transition(State::cleared());
        }
        let request_id = self.next_request_id();
        self.active_request = Some(request_id);
        self.emit(SearchEffect::Schedule {
            request_id,
            delay_ms: SEARCH_DEBOUNCE_MS,
        });
        self.notify();
        Transition(State::querying(request_id))
    }

    fn close(&mut self) -> Outcome<State> {
        self.cancel_active();
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
        self.scanned = 0;
        self.truncated = false;
        self.partial = false;
        self.notify();
        Transition(State::closed())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let len = self.hits.len();
        let current = self.selected.min(len - 1);
        let offset = delta.unsigned_abs() % len;
        let next = if delta >= 0 {
            (current + offset) % len
        } else if current >= offset {
            current - offset
        } else {
            len - (offset - current)
        };
        self.selected = next;
        self.notify();
    }

    fn select(&mut self, index: usize) {
        if self.hits.is_empty() {
            return;
        }
        let next = index.min(self.hits.len() - 1);
        if next == self.selected {
            return;
        }
        self.selected = next;
        self.notify();
    }

    /// Records a query the user acted on. Only a query that reached results is
    /// worth repeating, and a query that differs only in case is the same search,
    /// so it moves to the front instead of appearing twice.
    fn commit_recent(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        self.recents
            .retain(|recent| !recent.eq_ignore_ascii_case(query));
        self.recents.insert(0, query.to_owned());
        self.recents.truncate(SEARCH_RECENT_LIMIT);
        self.notify();
    }
}

impl State {
    pub fn phase(&self) -> SearchPhase {
        match self {
            State::Closed {} => SearchPhase::Closed,
            State::Cleared {} => SearchPhase::Cleared,
            State::Querying { .. } => SearchPhase::Query,
            State::Results {} => SearchPhase::Results,
            State::Error { .. } => SearchPhase::Error,
        }
    }

    pub fn failure_reason(&self) -> Option<&str> {
        match self {
            State::Error { reason } => Some(reason),
            _ => None,
        }
    }
}

#[state_machine(
    initial = "State::closed()",
    state(derive(Debug)),
    superstate(derive(Debug))
)]
impl SearchMachine {
    #[state]
    fn closed(&mut self, event: &SearchEvent) -> Outcome<State> {
        match event {
            SearchEvent::Open => self.open(),
            SearchEvent::Recall { query } => self.replace_query(query.clone()),
            _ => Handled,
        }
    }

    #[state]
    fn cleared(&mut self, event: &SearchEvent) -> Outcome<State> {
        match event {
            SearchEvent::Open => self.open(),
            SearchEvent::QueryChanged { query } => self.replace_query(query.clone()),
            SearchEvent::Recall { query } => self.replace_query(query.clone()),
            SearchEvent::Committed { query } => {
                self.commit_recent(query);
                Handled
            }
            SearchEvent::Close => self.close(),
            _ => Handled,
        }
    }

    #[state(local_storage("request_id: u64"))]
    fn querying(&mut self, event: &SearchEvent) -> Outcome<State> {
        match event {
            SearchEvent::Open => self.open(),
            SearchEvent::QueryChanged { query } => self.replace_query(query.clone()),
            SearchEvent::Recall { query } => self.replace_query(query.clone()),
            SearchEvent::Committed { query } => {
                self.commit_recent(query);
                Handled
            }
            SearchEvent::DebounceElapsed { request_id }
                if self.active_request == Some(*request_id) && !self.request_started =>
            {
                self.request_started = true;
                self.emit(SearchEffect::Query {
                    request_id: *request_id,
                    query: self.query.clone(),
                });
                self.notify();
                Handled
            }
            SearchEvent::Results {
                request_id,
                hits,
                scanned,
                truncated,
                partial,
            } if self.active_request == Some(*request_id) && self.request_started => {
                self.active_request = None;
                self.request_started = false;
                self.hits = hits.iter().take(SEARCH_RESULT_LIMIT).cloned().collect();
                self.selected = 0;
                self.scanned = *scanned;
                self.truncated = *truncated;
                self.partial = *partial;
                self.notify();
                Transition(State::results())
            }
            SearchEvent::Error { request_id, reason }
                if self.active_request == Some(*request_id) && self.request_started =>
            {
                self.active_request = None;
                self.request_started = false;
                self.hits.clear();
                self.selected = 0;
                self.scanned = 0;
                self.truncated = false;
                self.partial = false;
                self.notify();
                Transition(State::error(reason.clone()))
            }
            SearchEvent::Close => self.close(),
            _ => Handled,
        }
    }

    #[state]
    fn results(&mut self, event: &SearchEvent) -> Outcome<State> {
        match event {
            SearchEvent::Open => self.open(),
            SearchEvent::QueryChanged { query } => self.replace_query(query.clone()),
            SearchEvent::Recall { query } => self.replace_query(query.clone()),
            SearchEvent::Committed { query } => {
                self.commit_recent(query);
                Handled
            }
            SearchEvent::Select { index } => {
                self.select(*index);
                Handled
            }
            SearchEvent::MoveSelection { delta } => {
                self.move_selection(*delta);
                Handled
            }
            SearchEvent::Close => self.close(),
            _ => Handled,
        }
    }

    #[state(local_storage("reason: String"))]
    fn error(&mut self, event: &SearchEvent) -> Outcome<State> {
        match event {
            SearchEvent::Open => self.open(),
            SearchEvent::QueryChanged { query } => self.replace_query(query.clone()),
            SearchEvent::Recall { query } => self.replace_query(query.clone()),
            SearchEvent::Committed { query } => {
                self.commit_recent(query);
                Handled
            }
            SearchEvent::Close => self.close(),
            _ => Handled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use statig::blocking::StateMachine;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn resource() -> ResourceEntry {
        ResourceEntry {
            group: String::new(),
            version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            plural: "pods".to_owned(),
            scope: crate::discovery::ResourceScope::Namespaced,
            verbs: vec!["list".to_owned()],
            short_names: Vec::new(),
        }
    }

    fn hit(name: &str) -> SearchHit {
        SearchHit {
            resource: resource(),
            object: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": name,
                        "namespace": "default",
                        "uid": format!("uid-{name}"),
                    },
                }))
                .expect("valid pod"),
            ),
        }
    }

    struct Harness {
        machine: StateMachine<SearchMachine>,
        effects: UnboundedReceiver<SearchEffect>,
    }

    impl Harness {
        fn new() -> Self {
            let (effects, receiver) = unbounded_channel();
            Self {
                machine: SearchMachine::new(effects).state_machine(),
                effects: receiver,
            }
        }

        fn send(&mut self, event: &SearchEvent) -> Vec<SearchEffect> {
            self.machine.handle(event);
            let mut effects = Vec::new();
            while let Ok(effect) = self.effects.try_recv() {
                effects.push(effect);
            }
            effects
        }

        fn phase(&self) -> SearchPhase {
            self.machine.state().phase()
        }

        fn request_from(effects: &[SearchEffect]) -> u64 {
            effects
                .iter()
                .find_map(|effect| match effect {
                    SearchEffect::Schedule { request_id, .. }
                    | SearchEffect::Query { request_id, .. } => Some(*request_id),
                    _ => None,
                })
                .expect("query effect")
        }
    }

    #[test]
    fn query_change_schedules_after_200ms() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let effects = harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        });
        assert_eq!(harness.phase(), SearchPhase::Query);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            SearchEffect::Schedule { request_id, delay_ms }
                if *request_id == 1 && *delay_ms >= 200
        )));
    }

    #[test]
    fn debounce_result_replaces_results_and_moves_selection() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let effects = harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        });
        let request_id = Harness::request_from(&effects);
        let debounce_effects = harness.send(&SearchEvent::DebounceElapsed { request_id });
        assert!(debounce_effects.iter().any(|effect| matches!(
            effect,
            SearchEffect::Query {
                request_id: id,
                query,
            } if *id == request_id && query == "web"
        )));
        assert_eq!(harness.phase(), SearchPhase::Query);
        harness.send(&SearchEvent::Results {
            request_id,
            hits: vec![hit("web"), hit("web-api"), hit("web-canary")],
            scanned: 3,
            truncated: false,
            partial: true,
        });
        assert_eq!(harness.phase(), SearchPhase::Results);
        assert_eq!(harness.machine.inner().hits().len(), 3);
        assert_eq!(harness.machine.inner().scanned(), 3);
        assert!(!harness.machine.inner().truncated());
        assert!(harness.machine.inner().partial());
        harness.send(&SearchEvent::MoveSelection { delta: 1 });
        assert_eq!(harness.machine.inner().selected_index(), Some(1));
        harness.send(&SearchEvent::MoveSelection { delta: -2 });
        assert_eq!(harness.machine.inner().selected_index(), Some(2));
    }

    #[test]
    fn duplicate_debounce_does_not_start_a_second_query() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let effects = harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        });
        let request_id = Harness::request_from(&effects);
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        let effects = harness.send(&SearchEvent::DebounceElapsed { request_id });
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, SearchEffect::Query { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn result_limit_metadata_survives_until_the_next_query() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let request_id = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "perf".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        harness.send(&SearchEvent::Results {
            request_id,
            hits: (0..SEARCH_RESULT_LIMIT + 1)
                .map(|index| hit(&format!("perf-{index}")))
                .collect(),
            scanned: 5_000,
            truncated: true,
            partial: true,
        });
        assert_eq!(harness.machine.inner().hits().len(), SEARCH_RESULT_LIMIT);
        assert_eq!(harness.machine.inner().scanned(), 5_000);
        assert!(harness.machine.inner().truncated());
        assert!(harness.machine.inner().partial());
        harness.send(&SearchEvent::QueryChanged {
            query: "perf-2".to_owned(),
        });
        assert_eq!(harness.machine.inner().scanned(), 0);
        assert!(!harness.machine.inner().truncated());
        assert!(!harness.machine.inner().partial());
    }

    #[test]
    fn select_clamps_to_available_results() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let request_id = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        harness.send(&SearchEvent::Results {
            request_id,
            hits: vec![hit("web"), hit("web-api")],
            scanned: 2,
            truncated: true,
            partial: false,
        });
        harness.send(&SearchEvent::Select { index: 99 });
        assert_eq!(harness.machine.inner().selected_index(), Some(1));
        assert_eq!(harness.machine.inner().scanned(), 2);
        assert!(harness.machine.inner().truncated());
    }

    #[test]
    fn new_query_cancels_old_and_old_result_cannot_replace_new_results() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let first = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed { request_id: first });
        let second = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "api".to_owned(),
        }));
        assert_ne!(first, second);
        assert!(
            harness
                .send(&SearchEvent::Results {
                    request_id: first,
                    hits: vec![hit("old")],
                    scanned: 1,
                    truncated: false,
                    partial: false,
                })
                .iter()
                .all(|effect| *effect != SearchEffect::Notify)
        );
        assert!(
            harness
                .send(&SearchEvent::Error {
                    request_id: first,
                    reason: "old failure".to_owned(),
                })
                .iter()
                .all(|effect| *effect != SearchEffect::Notify)
        );
        assert_eq!(harness.phase(), SearchPhase::Query);
        harness.send(&SearchEvent::DebounceElapsed { request_id: second });
        harness.send(&SearchEvent::Results {
            request_id: second,
            hits: vec![hit("new")],
            scanned: 1,
            truncated: false,
            partial: false,
        });
        assert_eq!(
            harness.machine.inner().hits()[0]
                .object
                .metadata
                .name
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn results_before_debounce_completion_are_ignored() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let request_id = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        assert!(
            harness
                .send(&SearchEvent::Results {
                    request_id,
                    hits: vec![hit("too-early")],
                    scanned: 1,
                    truncated: false,
                    partial: false,
                })
                .is_empty()
        );
        assert_eq!(harness.phase(), SearchPhase::Query);
        assert!(harness.machine.inner().hits().is_empty());
    }

    #[test]
    fn failure_and_empty_query_are_explicit_terminal_events() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let request_id = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        harness.send(&SearchEvent::Error {
            request_id,
            reason: "offline".to_owned(),
        });
        assert_eq!(harness.phase(), SearchPhase::Error);
        assert_eq!(harness.machine.state().failure_reason(), Some("offline"));
        let active_request = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed {
            request_id: active_request,
        });
        let effects = harness.send(&SearchEvent::QueryChanged {
            query: String::new(),
        });
        assert_eq!(harness.phase(), SearchPhase::Cleared);
        assert!(effects.contains(&SearchEffect::Cancel {
            request_id: active_request,
        }));
        assert!(effects.contains(&SearchEffect::Notify));
        assert!(harness.machine.inner().query().is_empty());
        assert!(harness.machine.inner().hits().is_empty());
        assert_eq!(harness.machine.inner().scanned(), 0);
        assert!(!harness.machine.inner().truncated());
        assert!(!harness.machine.inner().partial());
    }

    #[test]
    fn close_cancels_active_request_and_ignores_late_results() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        let request_id = Harness::request_from(&harness.send(&SearchEvent::QueryChanged {
            query: "web".to_owned(),
        }));
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        let effects = harness.send(&SearchEvent::Close);
        assert_eq!(harness.phase(), SearchPhase::Closed);
        assert!(effects.contains(&SearchEffect::Cancel { request_id }));
        assert!(
            harness
                .send(&SearchEvent::Results {
                    request_id,
                    hits: vec![hit("late")],
                    scanned: 1,
                    truncated: false,
                    partial: false,
                })
                .is_empty()
        );
        assert!(harness.machine.inner().hits().is_empty());
    }

    fn search_for(harness: &mut Harness, query: &str) {
        let effects = harness.send(&SearchEvent::QueryChanged {
            query: query.to_owned(),
        });
        let request_id = Harness::request_from(&effects);
        harness.send(&SearchEvent::DebounceElapsed { request_id });
        harness.send(&SearchEvent::Results {
            request_id,
            hits: vec![hit(query)],
            scanned: 1,
            truncated: false,
            partial: false,
        });
    }

    #[test]
    fn a_committed_query_becomes_a_recent_and_moves_to_the_front() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        assert!(harness.machine.inner().recents().is_empty());

        search_for(&mut harness, "web");
        harness.send(&SearchEvent::Committed {
            query: "web".to_owned(),
        });
        search_for(&mut harness, "api");
        harness.send(&SearchEvent::Committed {
            query: "API".to_owned(),
        });
        assert_eq!(
            harness.machine.inner().recents().to_vec(),
            vec!["API".to_owned(), "web".to_owned()],
            "a repeated search moves to the front instead of appearing twice"
        );

        harness.send(&SearchEvent::Close);
        harness.send(&SearchEvent::Open);
        assert_eq!(
            harness.machine.inner().recents().to_vec(),
            vec!["API".to_owned(), "web".to_owned()],
            "recent searches survive closing and reopening the panel"
        );
    }

    #[test]
    fn recents_are_capped_and_never_empty() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        for index in 0..SEARCH_RECENT_LIMIT + 3 {
            harness.send(&SearchEvent::Committed {
                query: format!("web-{index}"),
            });
        }
        let recents = harness.machine.inner().recents();
        assert_eq!(recents.len(), SEARCH_RECENT_LIMIT);
        assert_eq!(recents[0], format!("web-{}", SEARCH_RECENT_LIMIT + 2));
        assert_eq!(recents[SEARCH_RECENT_LIMIT - 1], "web-3");

        harness.send(&SearchEvent::Committed {
            query: "   ".to_owned(),
        });
        assert_eq!(
            harness.machine.inner().recents().len(),
            SEARCH_RECENT_LIMIT,
            "a blank query is not a search"
        );
    }

    #[test]
    fn a_recalled_recent_runs_through_the_query_path() {
        let mut harness = Harness::new();
        harness.send(&SearchEvent::Open);
        harness.send(&SearchEvent::Committed {
            query: "web".to_owned(),
        });
        let effects = harness.send(&SearchEvent::Recall {
            query: "web".to_owned(),
        });
        assert_eq!(harness.phase(), SearchPhase::Query);
        assert_eq!(harness.machine.inner().query(), "web");
        let request_id = Harness::request_from(&effects);
        assert_eq!(effects.last(), Some(&SearchEffect::Notify));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            SearchEffect::Schedule { request_id: id, .. } if *id == request_id
        )));
    }
}
