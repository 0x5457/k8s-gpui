use statig::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

/// Connection lifecycle events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// Start a connection.
    Connect,
    /// The latest health check succeeded.
    HealthOk,
    /// The latest health check failed.
    HealthFail { reason: String },
    /// Disconnect and ignore later health results.
    Disconnect,
    /// Retry after backoff or a user request.
    Retry,
}

/// Effects executed by the UI or Tokio layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEffect {
    /// Check `/readyz` and return `HealthOk` or `HealthFail`.
    CheckHealth,
    /// Schedule a delayed `Retry`. Consecutive failures increase the delay.
    ScheduleRetry { delay_ms: u64 },
    /// Notify the UI after a state change.
    Notify,
}

/// Shared state contains the effect channel and failure count.
pub struct ConnectionMachine {
    effects: UnboundedSender<ConnectionEffect>,
    retry_attempt: u32,
}

impl ConnectionMachine {
    pub fn new(effects: UnboundedSender<ConnectionEffect>) -> Self {
        Self {
            effects,
            retry_attempt: 0,
        }
    }

    fn emit(&self, effect: ConnectionEffect) {
        let _ = self.effects.send(effect);
    }

    /// Record a failure and return the next backoff delay.
    fn next_retry_delay_ms(&mut self) -> u64 {
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        // The counter is incremented above, so it already holds the number of
        // failures the shared ladder counts from.
        let delay = crate::latency::backoff(self.retry_attempt);
        delay.as_millis() as u64
    }
}

#[state_machine(
    initial = "State::unknown()",
    state(derive(Debug)),
    superstate(derive(Debug))
)]
impl ConnectionMachine {
    #[state]
    fn unknown(&mut self, event: &ConnectionEvent) -> Outcome<State> {
        match event {
            ConnectionEvent::Connect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::CheckHealth);
                self.emit(ConnectionEffect::Notify);
                Transition(State::connecting())
            }
            ConnectionEvent::Disconnect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::offline())
            }
            _ => Handled,
        }
    }

    #[state]
    fn connecting(&mut self, event: &ConnectionEvent) -> Outcome<State> {
        match event {
            ConnectionEvent::HealthOk => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::ready())
            }
            ConnectionEvent::HealthFail { reason } => {
                let delay_ms = self.next_retry_delay_ms();
                self.emit(ConnectionEffect::ScheduleRetry { delay_ms });
                self.emit(ConnectionEffect::Notify);
                Transition(State::degraded(reason.clone()))
            }
            ConnectionEvent::Disconnect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::offline())
            }
            ConnectionEvent::Connect | ConnectionEvent::Retry => Handled,
        }
    }

    #[state]
    fn ready(&mut self, event: &ConnectionEvent) -> Outcome<State> {
        match event {
            ConnectionEvent::HealthFail { reason } => {
                let delay_ms = self.next_retry_delay_ms();
                self.emit(ConnectionEffect::ScheduleRetry { delay_ms });
                self.emit(ConnectionEffect::Notify);
                Transition(State::degraded(reason.clone()))
            }
            ConnectionEvent::Disconnect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::offline())
            }
            _ => Handled,
        }
    }

    #[state(local_storage("reason: String"))]
    fn degraded(&mut self, event: &ConnectionEvent) -> Outcome<State> {
        match event {
            ConnectionEvent::Connect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::CheckHealth);
                self.emit(ConnectionEffect::Notify);
                Transition(State::connecting())
            }
            ConnectionEvent::Retry => {
                self.emit(ConnectionEffect::CheckHealth);
                self.emit(ConnectionEffect::Notify);
                Transition(State::connecting())
            }
            ConnectionEvent::HealthOk => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::ready())
            }
            ConnectionEvent::HealthFail { reason } => {
                let delay_ms = self.next_retry_delay_ms();
                self.emit(ConnectionEffect::ScheduleRetry { delay_ms });
                self.emit(ConnectionEffect::Notify);
                Transition(State::degraded(reason.clone()))
            }
            ConnectionEvent::Disconnect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::Notify);
                Transition(State::offline())
            }
        }
    }

    #[state]
    fn offline(&mut self, event: &ConnectionEvent) -> Outcome<State> {
        match event {
            ConnectionEvent::Connect => {
                self.retry_attempt = 0;
                self.emit(ConnectionEffect::CheckHealth);
                self.emit(ConnectionEffect::Notify);
                Transition(State::connecting())
            }
            ConnectionEvent::Retry => {
                self.emit(ConnectionEffect::CheckHealth);
                self.emit(ConnectionEffect::Notify);
                Transition(State::connecting())
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
        Unknown,
        Connecting,
        Ready,
        Degraded(String),
        Offline,
    }

    fn state_spec(state: &State) -> ExpectedState {
        match state {
            State::Unknown {} => ExpectedState::Unknown,
            State::Connecting {} => ExpectedState::Connecting,
            State::Ready {} => ExpectedState::Ready,
            State::Degraded { reason } => ExpectedState::Degraded(reason.clone()),
            State::Offline {} => ExpectedState::Offline,
        }
    }

    struct Harness {
        machine: StateMachine<ConnectionMachine>,
        effects: UnboundedReceiver<ConnectionEffect>,
    }

    impl Harness {
        fn new() -> Self {
            let (effects_tx, effects_rx) = unbounded_channel();
            Self {
                machine: ConnectionMachine::new(effects_tx).state_machine(),
                effects: effects_rx,
            }
        }

        fn send(&mut self, event: &ConnectionEvent) -> Vec<ConnectionEffect> {
            self.machine.handle(event);
            let mut collected = Vec::new();
            while let Ok(effect) = self.effects.try_recv() {
                collected.push(effect);
            }
            collected
        }

        fn state(&self) -> ExpectedState {
            state_spec(self.machine.state())
        }

        fn reach(&mut self, target: &ExpectedState) {
            match target {
                ExpectedState::Unknown => {}
                ExpectedState::Connecting => {
                    self.send(&ConnectionEvent::Connect);
                }
                ExpectedState::Ready => {
                    self.send(&ConnectionEvent::Connect);
                    self.send(&ConnectionEvent::HealthOk);
                }
                ExpectedState::Degraded(reason) => {
                    self.send(&ConnectionEvent::Connect);
                    self.send(&ConnectionEvent::HealthFail {
                        reason: reason.clone(),
                    });
                }
                ExpectedState::Offline => {
                    self.send(&ConnectionEvent::Disconnect);
                }
            }
            assert_eq!(self.state(), target.clone());
        }
    }

    type Row = (ConnectionEvent, ExpectedState, Vec<ConnectionEffect>);

    /// Expected results for every state and event pair.
    fn cases(state: &ExpectedState) -> Vec<Row> {
        use ConnectionEffect::{CheckHealth, Notify, ScheduleRetry};
        use ConnectionEvent::{Connect, Disconnect, HealthFail, HealthOk, Retry};
        let fail = |reason: &str| HealthFail {
            reason: reason.to_string(),
        };
        match state {
            ExpectedState::Unknown => vec![
                (
                    Connect,
                    ExpectedState::Connecting,
                    vec![CheckHealth, Notify],
                ),
                (HealthOk, ExpectedState::Unknown, vec![]),
                (fail("boom"), ExpectedState::Unknown, vec![]),
                (Disconnect, ExpectedState::Offline, vec![Notify]),
                (Retry, ExpectedState::Unknown, vec![]),
            ],
            // retry_attempt is zero after reaching this state
            ExpectedState::Connecting => vec![
                (Connect, ExpectedState::Connecting, vec![]),
                (HealthOk, ExpectedState::Ready, vec![Notify]),
                (
                    fail("boom"),
                    ExpectedState::Degraded("boom".to_string()),
                    vec![ScheduleRetry { delay_ms: 1_000 }, Notify],
                ),
                (Disconnect, ExpectedState::Offline, vec![Notify]),
                (Retry, ExpectedState::Connecting, vec![]),
            ],
            ExpectedState::Ready => vec![
                (Connect, ExpectedState::Ready, vec![]),
                (HealthOk, ExpectedState::Ready, vec![]),
                (
                    fail("boom"),
                    ExpectedState::Degraded("boom".to_string()),
                    vec![ScheduleRetry { delay_ms: 1_000 }, Notify],
                ),
                (Disconnect, ExpectedState::Offline, vec![Notify]),
                (Retry, ExpectedState::Ready, vec![]),
            ],
            // retry_attempt is one after reaching this state
            ExpectedState::Degraded(_) => vec![
                (
                    Connect,
                    ExpectedState::Connecting,
                    vec![CheckHealth, Notify],
                ),
                (HealthOk, ExpectedState::Ready, vec![Notify]),
                (
                    fail("boom2"),
                    ExpectedState::Degraded("boom2".to_string()),
                    vec![ScheduleRetry { delay_ms: 2_000 }, Notify],
                ),
                (Disconnect, ExpectedState::Offline, vec![Notify]),
                (Retry, ExpectedState::Connecting, vec![CheckHealth, Notify]),
            ],
            ExpectedState::Offline => vec![
                (
                    Connect,
                    ExpectedState::Connecting,
                    vec![CheckHealth, Notify],
                ),
                (HealthOk, ExpectedState::Offline, vec![]),
                (fail("boom"), ExpectedState::Offline, vec![]),
                (Disconnect, ExpectedState::Offline, vec![]),
                (Retry, ExpectedState::Connecting, vec![CheckHealth, Notify]),
            ],
        }
    }

    #[test]
    fn transition_matrix_covers_every_state_event_pair() {
        let states = [
            ExpectedState::Unknown,
            ExpectedState::Connecting,
            ExpectedState::Ready,
            ExpectedState::Degraded("boom".to_string()),
            ExpectedState::Offline,
        ];

        let mut checked = 0;
        for state in &states {
            let mut probe = Harness::new();
            probe.reach(state);
            let rows = cases(state);
            assert_eq!(rows.len(), 5, "every event has one row: {state:?}");

            for (event, expected_state, expected_effects) in rows {
                let mut harness = Harness::new();
                harness.reach(state);
                let effects = harness.send(&event);
                assert_eq!(harness.state(), expected_state, "{event:?} in {state:?}");
                assert_eq!(effects, expected_effects, "{event:?} in {state:?}");
                checked += 1;
            }
        }
        assert_eq!(checked, states.len() * 5);
    }

    #[test]
    fn retry_backoff_grows_caps_and_resets() {
        let mut harness = Harness::new();
        harness.send(&ConnectionEvent::Connect);

        let mut delays = Vec::new();
        for _ in 0..8 {
            let effects = harness.send(&ConnectionEvent::HealthFail {
                reason: "down".to_string(),
            });
            match effects.first() {
                Some(ConnectionEffect::ScheduleRetry { delay_ms }) => delays.push(*delay_ms),
                other => panic!("expected ScheduleRetry, got {other:?}"),
            }
        }
        assert_eq!(
            delays,
            [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]
        );

        // One success clears backoff. The next failure starts at the minimum.
        harness.send(&ConnectionEvent::HealthOk);
        let effects = harness.send(&ConnectionEvent::HealthFail {
            reason: "down".to_string(),
        });
        assert_eq!(
            effects.first(),
            Some(&ConnectionEffect::ScheduleRetry { delay_ms: 1_000 })
        );
    }

    #[test]
    fn degrade_and_recover_keeps_last_reason() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Degraded("boom".to_string()));

        harness.send(&ConnectionEvent::HealthFail {
            reason: "timeout".to_string(),
        });
        assert_eq!(
            harness.state(),
            ExpectedState::Degraded("timeout".to_string())
        );

        harness.send(&ConnectionEvent::HealthOk);
        assert_eq!(harness.state(), ExpectedState::Ready);
    }

    fn arb_event() -> impl Strategy<Value = ConnectionEvent> {
        prop_oneof![
            Just(ConnectionEvent::Connect),
            Just(ConnectionEvent::HealthOk),
            "[a-z]{0,8}".prop_map(|reason| ConnectionEvent::HealthFail { reason }),
            Just(ConnectionEvent::Disconnect),
            Just(ConnectionEvent::Retry),
        ]
    }

    proptest! {
        #[test]
        fn random_sequences_keep_invariants(events in prop::collection::vec(arb_event(), 0..200)) {
            let mut harness = Harness::new();
            let mut previous = harness.state();

            for event in &events {
                let effects = harness.send(event);
                let next = harness.state();

                for effect in &effects {
                    if let ConnectionEffect::ScheduleRetry { delay_ms } = effect {
                        prop_assert!(
                            (1_000_u64..=30_000_u64).contains(delay_ms),
                            "the shared ladder runs 1s to 30s"
                        );
                    }
                }
                if matches!(previous, ExpectedState::Unknown | ExpectedState::Offline)
                    && matches!(event, ConnectionEvent::HealthFail { .. })
                {
                    prop_assert!(next == previous, "health failure while disconnected does not change state");
                    prop_assert!(effects.is_empty(), "health failure while disconnected sends no effect");
                }
                if previous == ExpectedState::Offline && next != ExpectedState::Offline {
                    prop_assert!(matches!(event, ConnectionEvent::Connect | ConnectionEvent::Retry));
                }
                previous = next;
            }
        }
    }
}
