//! Combines user and terminal cursor blink settings for the view timer.

use std::time::Duration;

/// Cursor blink interval.
pub const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Cursor visibility from the user setting, terminal request, and blink phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlinkState {
    setting: bool,
    terminal: bool,
    visible: bool,
}

impl Default for BlinkState {
    fn default() -> Self {
        Self {
            setting: true,
            terminal: true,
            visible: true,
        }
    }
}

impl BlinkState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports whether the cursor blinks. Otherwise, it stays visible.
    pub fn active(&self) -> bool {
        self.setting && self.terminal
    }

    /// Reports whether the cursor is visible in the current blink phase.
    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_setting(&mut self, enabled: bool) {
        self.setting = enabled;
        if !self.active() {
            self.visible = true;
        }
    }

    pub fn set_terminal(&mut self, enabled: bool) {
        self.terminal = enabled;
        if !self.active() {
            self.visible = true;
        }
    }

    /// Makes the cursor visible until the next blink tick.
    pub fn show(&mut self) {
        self.visible = true;
    }

    /// Advances the blink phase and reports whether it changed.
    pub fn tick(&mut self) -> bool {
        if !self.active() {
            return false;
        }
        self.visible = !self.visible;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinks_by_default() {
        let state = BlinkState::new();
        assert!(state.active());
        assert!(state.visible());
    }

    #[test]
    fn tick_toggles_only_while_active() {
        let mut state = BlinkState::new();
        assert!(state.tick());
        assert!(!state.visible());
        assert!(state.tick());
        assert!(state.visible());

        state.set_setting(false);
        assert!(!state.active());
        assert!(
            state.visible(),
            "cursor stays visible when blinking is disabled"
        );
        assert!(!state.tick());
        assert!(state.visible());
    }

    #[test]
    fn terminal_request_disables_blinking_until_reenabled() {
        let mut state = BlinkState::new();
        state.set_terminal(false);
        assert!(!state.active());
        assert!(!state.tick());
        state.set_terminal(true);
        assert!(state.active());
        assert!(state.tick());
    }

    #[test]
    fn show_resets_phase() {
        let mut state = BlinkState::new();
        state.tick();
        assert!(!state.visible());
        state.show();
        assert!(state.visible());
    }
}
