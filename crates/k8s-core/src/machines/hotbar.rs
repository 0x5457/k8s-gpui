use statig::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::hotbar::{Hotbar, HotbarError, Slot};

/// Hotbar edit events. The machine does not perform I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotbarEvent {
    /// Replace the current hotbar with a loaded value.
    Load(Hotbar),
    /// Add a bank after trimming its name.
    CreateBank {
        name: String,
    },
    RenameBank {
        index: usize,
        name: String,
    },
    /// Remove a bank. Removing the last bank returns to `Empty`.
    RemoveBank {
        index: usize,
    },
    AddSlot {
        bank: usize,
        slot: Slot,
    },
    /// Set the active bank and persist the change.
    SwitchBank {
        index: usize,
    },
    /// Select a slot and notify without saving.
    SwitchSlot {
        bank: usize,
        slot: usize,
    },
}

/// Effects executed by the UI or Tokio layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotbarEffect {
    /// Persist the current hotbar atomically.
    Persist,
    /// Notify the UI after a state change or rejected operation.
    Notify { reason: Option<String> },
}

/// Shared state contains only the effect channel.
pub struct HotbarMachine {
    effects: UnboundedSender<HotbarEffect>,
}

impl HotbarMachine {
    pub fn new(effects: UnboundedSender<HotbarEffect>) -> Self {
        Self { effects }
    }

    fn emit(&self, effect: HotbarEffect) {
        let _ = self.effects.send(effect);
    }

    fn notify_ok(&self) {
        self.emit(HotbarEffect::Notify { reason: None });
    }

    fn notify_err(&self, error: &HotbarError) {
        self.emit(HotbarEffect::Notify {
            reason: Some(error.to_string()),
        });
    }

    /// Persist a successful change before notifying the UI.
    fn persisted(&self) {
        self.emit(HotbarEffect::Persist);
        self.notify_ok();
    }

    /// Reject operations that require a bank while the state is `Empty`.
    fn reject(&self, index: usize) {
        self.notify_err(&HotbarError::NoSuchBank { index });
    }
}

impl State {
    /// Hotbar data in `Banks`. `Empty` has no data.
    pub fn hotbar(&self) -> Option<&Hotbar> {
        match self {
            State::Banks { hotbar } => Some(hotbar),
            State::Empty {} => None,
        }
    }
}

#[state_machine(
    initial = "State::empty()",
    state(derive(Debug)),
    superstate(derive(Debug))
)]
impl HotbarMachine {
    #[state]
    fn empty(&mut self, event: &HotbarEvent) -> Outcome<State> {
        match event {
            HotbarEvent::Load(hotbar) => {
                self.notify_ok();
                if hotbar.banks.is_empty() {
                    Handled
                } else {
                    Transition(State::banks(hotbar.clone()))
                }
            }
            HotbarEvent::CreateBank { name } => {
                let mut hotbar = Hotbar::default();
                match hotbar.create_bank(name.clone()) {
                    Ok(_) => {
                        self.persisted();
                        Transition(State::banks(hotbar))
                    }
                    Err(error) => {
                        self.notify_err(&error);
                        Handled
                    }
                }
            }
            HotbarEvent::RenameBank { index, .. }
            | HotbarEvent::RemoveBank { index }
            | HotbarEvent::SwitchBank { index } => {
                self.reject(*index);
                Handled
            }
            HotbarEvent::AddSlot { bank, .. } | HotbarEvent::SwitchSlot { bank, .. } => {
                self.reject(*bank);
                Handled
            }
        }
    }

    #[state(local_storage("hotbar: Hotbar"))]
    fn banks(&mut self, event: &HotbarEvent, hotbar: &mut Hotbar) -> Outcome<State> {
        match event {
            HotbarEvent::Load(loaded) => {
                self.notify_ok();
                if loaded.banks.is_empty() {
                    Transition(State::empty())
                } else {
                    *hotbar = loaded.clone();
                    Handled
                }
            }
            HotbarEvent::CreateBank { name } => match hotbar.create_bank(name.clone()) {
                Ok(_) => {
                    self.persisted();
                    Handled
                }
                Err(error) => {
                    self.notify_err(&error);
                    Handled
                }
            },
            HotbarEvent::RenameBank { index, name } => {
                match hotbar.rename_bank(*index, name.clone()) {
                    Ok(()) => {
                        self.persisted();
                        Handled
                    }
                    Err(error) => {
                        self.notify_err(&error);
                        Handled
                    }
                }
            }
            HotbarEvent::RemoveBank { index } => match hotbar.remove_bank(*index) {
                Ok(_) => {
                    self.persisted();
                    if hotbar.banks.is_empty() {
                        Transition(State::empty())
                    } else {
                        Handled
                    }
                }
                Err(error) => {
                    self.notify_err(&error);
                    Handled
                }
            },
            HotbarEvent::AddSlot { bank, slot } => {
                match hotbar.add_slot(*bank, slot.cluster_id, slot.label.clone()) {
                    Ok(_) => {
                        self.persisted();
                        Handled
                    }
                    Err(error) => {
                        self.notify_err(&error);
                        Handled
                    }
                }
            }
            HotbarEvent::SwitchBank { index } => match hotbar.set_active(*index) {
                Ok(()) => {
                    self.persisted();
                    Handled
                }
                Err(error) => {
                    self.notify_err(&error);
                    Handled
                }
            },
            HotbarEvent::SwitchSlot { bank, slot } => {
                let error = match hotbar.bank(*bank) {
                    None => Some(HotbarError::NoSuchBank { index: *bank }),
                    Some(bank_ref) if *slot >= bank_ref.slots.len() => {
                        Some(HotbarError::NoSuchSlot {
                            bank: *bank,
                            slot: *slot,
                        })
                    }
                    Some(_) => None,
                };
                match error {
                    Some(error) => self.notify_err(&error),
                    None => self.notify_ok(),
                }
                Handled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterId;
    use crate::hotbar::MAX_SLOTS_PER_BANK;
    use proptest::prelude::*;
    use statig::blocking::StateMachine;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ExpectedState {
        Empty,
        Banks(Hotbar),
    }

    fn state_spec(state: &State) -> ExpectedState {
        match state {
            State::Empty {} => ExpectedState::Empty,
            State::Banks { hotbar } => ExpectedState::Banks(hotbar.clone()),
        }
    }

    fn id(context: &str) -> ClusterId {
        ClusterId::derive(context, "https://example.com:6443")
    }

    /// Fixed matrix fixture with two banks and `ops` active.
    fn fixture() -> Hotbar {
        let mut hotbar = Hotbar::default();
        let default = hotbar.create_bank("default").expect("create bank");
        hotbar
            .add_slot(default, id("alpha"), "alpha")
            .expect("add slot");
        let ops = hotbar.create_bank("ops").expect("create bank");
        hotbar.add_slot(ops, id("beta"), "beta").expect("add slot");
        hotbar
            .add_slot(ops, id("gamma"), "gamma")
            .expect("add slot");
        hotbar.set_active(1).expect("active = ops");
        hotbar
    }

    fn banks_after(edit: impl FnOnce(&mut Hotbar)) -> ExpectedState {
        let mut hotbar = fixture();
        edit(&mut hotbar);
        ExpectedState::Banks(hotbar)
    }

    fn notify_ok() -> HotbarEffect {
        HotbarEffect::Notify { reason: None }
    }

    fn notify_err(message: &str) -> HotbarEffect {
        HotbarEffect::Notify {
            reason: Some(message.to_string()),
        }
    }

    struct Harness {
        machine: StateMachine<HotbarMachine>,
        effects: UnboundedReceiver<HotbarEffect>,
    }

    impl Harness {
        fn new() -> Self {
            let (effects_tx, effects_rx) = unbounded_channel();
            Self {
                machine: HotbarMachine::new(effects_tx).state_machine(),
                effects: effects_rx,
            }
        }

        fn send(&mut self, event: &HotbarEvent) -> Vec<HotbarEffect> {
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
                ExpectedState::Empty => {}
                ExpectedState::Banks(hotbar) => {
                    self.send(&HotbarEvent::Load(hotbar.clone()));
                }
            }
            assert_eq!(self.state(), target.clone());
        }
    }

    type Row = (HotbarEvent, ExpectedState, Vec<HotbarEffect>);

    fn kind(event: &HotbarEvent) -> &'static str {
        match event {
            HotbarEvent::Load(_) => "Load",
            HotbarEvent::CreateBank { .. } => "CreateBank",
            HotbarEvent::RenameBank { .. } => "RenameBank",
            HotbarEvent::RemoveBank { .. } => "RemoveBank",
            HotbarEvent::AddSlot { .. } => "AddSlot",
            HotbarEvent::SwitchBank { .. } => "SwitchBank",
            HotbarEvent::SwitchSlot { .. } => "SwitchSlot",
        }
    }

    /// All event kinds in sorted order.
    const EVENT_KINDS: [&str; 7] = [
        "AddSlot",
        "CreateBank",
        "Load",
        "RemoveBank",
        "RenameBank",
        "SwitchBank",
        "SwitchSlot",
    ];

    /// Expected results for every state and event pair.
    fn cases(state: &ExpectedState) -> Vec<Row> {
        use HotbarEffect::Persist;

        match state {
            ExpectedState::Empty => vec![
                (
                    HotbarEvent::Load(Hotbar::default()),
                    ExpectedState::Empty,
                    vec![notify_ok()],
                ),
                (
                    HotbarEvent::Load(fixture()),
                    ExpectedState::Banks(fixture()),
                    vec![notify_ok()],
                ),
                (
                    HotbarEvent::CreateBank {
                        name: "default".to_string(),
                    },
                    ExpectedState::Banks({
                        let mut hotbar = Hotbar::default();
                        hotbar.create_bank("default").expect("create bank");
                        hotbar
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::CreateBank {
                        name: "  ".to_string(),
                    },
                    ExpectedState::Empty,
                    vec![notify_err("Bank name must not be empty. Enter a name.")],
                ),
                (
                    HotbarEvent::RenameBank {
                        index: 0,
                        name: "prod".to_string(),
                    },
                    ExpectedState::Empty,
                    vec![notify_err(
                        "Bank index 0 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::RemoveBank { index: 0 },
                    ExpectedState::Empty,
                    vec![notify_err(
                        "Bank index 0 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::AddSlot {
                        bank: 0,
                        slot: Slot::new(id("alpha"), "alpha"),
                    },
                    ExpectedState::Empty,
                    vec![notify_err(
                        "Bank index 0 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::SwitchBank { index: 0 },
                    ExpectedState::Empty,
                    vec![notify_err(
                        "Bank index 0 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::SwitchSlot { bank: 0, slot: 0 },
                    ExpectedState::Empty,
                    vec![notify_err(
                        "Bank index 0 is out of range. Choose an existing bank.",
                    )],
                ),
            ],
            ExpectedState::Banks(_) => vec![
                (
                    HotbarEvent::Load(fixture()),
                    ExpectedState::Banks(fixture()),
                    vec![notify_ok()],
                ),
                (
                    HotbarEvent::Load(Hotbar::default()),
                    ExpectedState::Empty,
                    vec![notify_ok()],
                ),
                (
                    HotbarEvent::CreateBank {
                        name: "dev".to_string(),
                    },
                    banks_after(|hotbar| {
                        hotbar.create_bank("dev").expect("create bank");
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::CreateBank {
                        name: "ops".to_string(),
                    },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank name already exists: ops. Choose another name.",
                    )],
                ),
                (
                    HotbarEvent::RenameBank {
                        index: 0,
                        name: "prod".to_string(),
                    },
                    banks_after(|hotbar| {
                        hotbar.rename_bank(0, "prod").expect("rename");
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::RenameBank {
                        index: 0,
                        name: "ops".to_string(),
                    },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank name already exists: ops. Choose another name.",
                    )],
                ),
                (
                    HotbarEvent::RenameBank {
                        index: 7,
                        name: "prod".to_string(),
                    },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank index 7 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::RemoveBank { index: 0 },
                    banks_after(|hotbar| {
                        hotbar.remove_bank(0).expect("remove bank");
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::RemoveBank { index: 7 },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank index 7 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::AddSlot {
                        bank: 0,
                        slot: Slot::new(id("delta"), "delta"),
                    },
                    banks_after(|hotbar| {
                        hotbar.add_slot(0, id("delta"), "delta").expect("add slot");
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::AddSlot {
                        bank: 0,
                        slot: Slot::new(id("alpha"), "dup"),
                    },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "The cluster is already in bank default. Choose another bank.",
                    )],
                ),
                (
                    HotbarEvent::AddSlot {
                        bank: 9,
                        slot: Slot::new(id("delta"), "delta"),
                    },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank index 9 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::SwitchBank { index: 0 },
                    banks_after(|hotbar| {
                        hotbar.set_active(0).expect("switch bank");
                    }),
                    vec![Persist, notify_ok()],
                ),
                (
                    HotbarEvent::SwitchBank { index: 9 },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank index 9 is out of range. Choose an existing bank.",
                    )],
                ),
                (
                    HotbarEvent::SwitchSlot { bank: 1, slot: 1 },
                    ExpectedState::Banks(fixture()),
                    vec![notify_ok()],
                ),
                (
                    HotbarEvent::SwitchSlot { bank: 1, slot: 4 },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Slot index 4 is out of range for bank 1. Choose an existing slot.",
                    )],
                ),
                (
                    HotbarEvent::SwitchSlot { bank: 9, slot: 0 },
                    ExpectedState::Banks(fixture()),
                    vec![notify_err(
                        "Bank index 9 is out of range. Choose an existing bank.",
                    )],
                ),
            ],
        }
    }

    #[test]
    fn transition_matrix_covers_every_state_event_pair() {
        let states = [ExpectedState::Empty, ExpectedState::Banks(fixture())];

        let total: usize = states.iter().map(|state| cases(state).len()).sum();
        let mut checked = 0;
        for state in &states {
            let mut probe = Harness::new();
            probe.reach(state);
            let rows = cases(state);

            let mut kinds: Vec<&str> = rows.iter().map(|(event, ..)| kind(event)).collect();
            kinds.sort_unstable();
            kinds.dedup();
            assert_eq!(
                kinds, EVENT_KINDS,
                "every event has at least one row: {state:?}"
            );

            for (event, expected_state, expected_effects) in rows {
                let mut harness = Harness::new();
                harness.reach(state);
                let effects = harness.send(&event);
                assert_eq!(harness.state(), expected_state, "{event:?} in {state:?}");
                assert_eq!(effects, expected_effects, "{event:?} in {state:?}");
                checked += 1;
            }
        }
        assert_eq!(checked, total);
    }

    #[test]
    fn empty_only_accepts_load_and_create_bank() {
        let mut harness = Harness::new();
        assert_eq!(harness.state(), ExpectedState::Empty);

        let effects = harness.send(&HotbarEvent::CreateBank {
            name: "default".to_string(),
        });
        assert_eq!(effects, vec![HotbarEffect::Persist, notify_ok()]);
        let mut expected = Hotbar::default();
        expected.create_bank("default").expect("create bank");
        assert_eq!(
            harness.state(),
            ExpectedState::Banks(expected),
            "CreateBank enters Banks"
        );
    }

    #[test]
    fn remove_last_bank_returns_to_empty() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Banks(fixture()));
        harness.send(&HotbarEvent::RemoveBank { index: 0 });
        let effects = harness.send(&HotbarEvent::RemoveBank { index: 0 });
        assert_eq!(effects, vec![HotbarEffect::Persist, notify_ok()]);
        assert_eq!(harness.state(), ExpectedState::Empty);
    }

    #[test]
    fn load_replaces_and_can_clear_state() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Banks(fixture()));

        let mut replacement = Hotbar::default();
        replacement.create_bank("solo").expect("create bank");
        let effects = harness.send(&HotbarEvent::Load(replacement.clone()));
        assert_eq!(effects, vec![notify_ok()], "Load does not write to disk");
        assert_eq!(harness.state(), ExpectedState::Banks(replacement));

        let effects = harness.send(&HotbarEvent::Load(Hotbar::default()));
        assert_eq!(effects, vec![notify_ok()]);
        assert_eq!(harness.state(), ExpectedState::Empty);
    }

    #[test]
    fn slot_limit_is_twelve_and_rejection_keeps_state() {
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("full").expect("create bank");
        for position in 0..MAX_SLOTS_PER_BANK {
            hotbar
                .add_slot(bank, id(&format!("ctx-{position}")), "x")
                .expect("12 slots are accepted");
        }
        let expected = ExpectedState::Banks(hotbar);

        let mut harness = Harness::new();
        harness.reach(&expected);
        let effects = harness.send(&HotbarEvent::AddSlot {
            bank,
            slot: Slot::new(id("overflow"), "overflow"),
        });
        assert_eq!(
            effects,
            vec![notify_err(
                "Bank full is full. It has 12 slots. Remove a slot and try again."
            )]
        );
        assert_eq!(
            harness.state(),
            expected,
            "the 13th slot does not change state or write to disk"
        );
    }

    #[test]
    fn switch_slot_only_validates_and_never_persists() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Banks(fixture()));

        let effects = harness.send(&HotbarEvent::SwitchSlot { bank: 0, slot: 0 });
        assert_eq!(effects, vec![notify_ok()]);
        assert_eq!(harness.state(), ExpectedState::Banks(fixture()));

        let effects = harness.send(&HotbarEvent::SwitchSlot { bank: 1, slot: 3 });
        assert_eq!(
            effects,
            vec![notify_err(
                "Slot index 3 is out of range for bank 1. Choose an existing slot."
            )]
        );
        assert_eq!(harness.state(), ExpectedState::Banks(fixture()));
    }

    #[test]
    fn active_follows_bank_removal() {
        let mut harness = Harness::new();
        harness.reach(&ExpectedState::Banks(fixture()));

        harness.send(&HotbarEvent::RemoveBank { index: 0 });
        let state = harness.state();
        let ExpectedState::Banks(hotbar) = state else {
            panic!("state is still Banks");
        };
        assert_eq!(
            hotbar.active, 0,
            "active still points to ops after the shift"
        );
        assert_eq!(
            hotbar.active_bank().map(|bank| bank.name.as_str()),
            Some("ops")
        );
    }

    fn arb_name() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("ops".to_string()),
            Just("default".to_string()),
            Just(" ".to_string()),
            "[a-z]{1,4}".prop_map(String::from),
        ]
    }

    fn arb_slot() -> impl Strategy<Value = Slot> {
        (0usize..5).prop_map(|position| {
            Slot::new(
                id(&format!("ctx-{position}")),
                format!("cluster-{position}"),
            )
        })
    }

    fn arb_event() -> impl Strategy<Value = HotbarEvent> {
        prop_oneof![
            Just(HotbarEvent::Load(fixture())),
            Just(HotbarEvent::Load(Hotbar::default())),
            arb_name().prop_map(|name| HotbarEvent::CreateBank { name }),
            (0usize..6, arb_name())
                .prop_map(|(index, name)| HotbarEvent::RenameBank { index, name }),
            (0usize..6).prop_map(|index| HotbarEvent::RemoveBank { index }),
            (0usize..6, arb_slot()).prop_map(|(bank, slot)| HotbarEvent::AddSlot { bank, slot }),
            (0usize..6).prop_map(|index| HotbarEvent::SwitchBank { index }),
            (0usize..6, 0usize..4).prop_map(|(bank, slot)| HotbarEvent::SwitchSlot { bank, slot }),
        ]
    }

    proptest! {
        #[test]
        fn random_sequences_keep_invariants(events in prop::collection::vec(arb_event(), 0..150)) {
            let mut harness = Harness::new();

            for event in &events {
                let before = harness.state();
                let effects = harness.send(event);
                let after = harness.state();

                prop_assert!(
                    effects.iter().any(|effect| matches!(effect, HotbarEffect::Notify { .. })),
                    "every event sends at least one Notify: {event:?}"
                );
                if !effects.contains(&HotbarEffect::Persist)
                    && !matches!(event, HotbarEvent::Load(_))
                {
                    prop_assert_eq!(
                        &after,
                        &before,
                        "changes without Persist keep the data unchanged: {:?}",
                        event
                    );
                }
                if effects
                    .iter()
                    .any(|effect| matches!(effect, HotbarEffect::Notify { reason: Some(_) }))
                {
                    prop_assert_eq!(&after, &before, "a rejected operation does not change state: {:?}", event);
                }

                match &after {
                    ExpectedState::Empty => {}
                    ExpectedState::Banks(hotbar) => {
                        prop_assert!(!hotbar.banks.is_empty(), "Banks has at least one bank");
                        prop_assert!(hotbar.active < hotbar.banks.len(), "active is always valid");
                        for bank in &hotbar.banks {
                            prop_assert!(!bank.name.trim().is_empty(), "bank name is not empty");
                            prop_assert_eq!(bank.name.as_str(), bank.name.trim(), "bank name is trimmed");
                            prop_assert!(bank.slots.len() <= MAX_SLOTS_PER_BANK, "slot limit");
                        }
                        for (position, bank) in hotbar.banks.iter().enumerate() {
                            for other in &hotbar.banks[position + 1..] {
                                prop_assert_ne!(&bank.name, &other.name, "bank names are unique");
                            }
                        }
                    }
                }
            }
        }
    }
}
