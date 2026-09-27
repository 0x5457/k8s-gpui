use std::sync::Arc;

use kube::core::DynamicObject;
use statig::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::discovery::ResourceEntry;

pub const SEARCH_DEBOUNCE_MS: u64 = 200;
pub const SEARCH_RESULT_LIMIT: usize = 50;
/// Recent queries offered when the field is empty, newest first.
pub(crate) const SEARCH_RECENT_LIMIT: usize = 5;

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
        self.reset_results();
        self.notify();
        Transition(State::cleared())
    }

    fn replace_query(&mut self, query: String) -> Outcome<State> {
        self.cancel_active();
        self.query = query.trim().to_owned();
        self.reset_results();
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
        self.reset_results();
        self.notify();
        Transition(State::closed())
    }

    /// Everything a new query invalidates, and nothing that belongs to the query.
    ///
    /// Opening, closing and replacing a query each had this list written out, and
    /// the fields are the ones a stale result would be read through - so a field
    /// added here and forgotten in one of the three would show the previous
    /// query's hits under the new query's text.
    fn reset_results(&mut self) {
        self.hits.clear();
        self.selected = 0;
        self.scanned = 0;
        self.truncated = false;
        self.partial = false;
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
