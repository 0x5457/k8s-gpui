use std::rc::Rc;

use gpui::App;

pub use k8s_core::update::{UpdatePhase, UpdateState as UpdateUiState};

pub type UpdateCallback = Rc<dyn Fn(&mut App)>;

#[derive(Clone)]
pub struct UpdateActions {
    pub check: UpdateCallback,
    pub retry: UpdateCallback,
    pub restart: UpdateCallback,
}

impl UpdateActions {
    pub fn new(
        check: impl Fn(&mut App) + 'static,
        retry: impl Fn(&mut App) + 'static,
        restart: impl Fn(&mut App) + 'static,
    ) -> Self {
        Self {
            check: Rc::new(check),
            retry: Rc::new(retry),
            restart: Rc::new(restart),
        }
    }

    pub fn run_check(&self, cx: &mut App) {
        (self.check)(cx);
    }

    pub fn run_retry(&self, cx: &mut App) {
        (self.retry)(cx);
    }

    pub fn run_restart(&self, cx: &mut App) {
        (self.restart)(cx);
    }
}

/// The one update notice a run may open on its own.
///
/// `main` only configures the updater for a managed install, so a build that cannot update
/// itself reaches the shell with the reason and no actions: a debug build, a build without a
/// signing key, or one with updates turned off. Its notice offers nothing the user can act on
/// and returns on every launch, so it never opens on its own. Apple HIG `alerts.md` › Best
/// practices asks for exactly that: do not interrupt with an alert that only informs, and do
/// not show an alert when the app starts.
///
/// A build that can update gets the reason once. A repeat in the same run only covers the
/// table again, which is what suppression is for (HIG `alerts.md` › Desktop (macOS): macOS
/// lets people suppress subsequent occurrences of the same alert). Settings keeps the status
/// and the manual check reachable either way.
#[derive(Debug, Default)]
pub struct StartupNotice {
    claimed: bool,
}

impl StartupNotice {
    /// Claim this run's notice. Reports false when the build has no update actions, or when
    /// the run already spent its notice.
    pub fn claim(&mut self, actions_configured: bool) -> bool {
        if self.claimed || !actions_configured {
            return false;
        }
        self.claimed = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::TestAppContext;

    use super::*;

    #[test]
    fn status_copy_uses_version_and_progress() {
        let state = UpdateUiState::new(UpdatePhase::Downloading)
            .with_version("1.2.3")
            .with_progress(Some(0.426));
        assert_eq!(state.status_text(), "Downloading version 1.2.3 (43%)");
        assert_eq!(state.progress_percent(), Some(43));
    }

    #[test]
    fn ready_and_failure_copy_stays_actionable() {
        let ready = UpdateUiState::new(UpdatePhase::Ready).with_version("1.2.3");
        assert_eq!(ready.status_text(), "Version 1.2.3 is ready");

        let failed = UpdateUiState::new(UpdatePhase::Failed)
            .with_error("Download verification failed. Try again.");
        assert_eq!(
            failed.status_text(),
            "Download verification failed. Try again."
        );

        let unsupported = UpdateUiState::new(UpdatePhase::Unsupported)
            .with_error("This build is not managed by the updater.");
        assert_eq!(
            unsupported.status_text(),
            "Automatic updates are not available: This build is not managed by the updater."
        );
    }

    #[test]
    fn quiet_states_stay_out_of_the_strip() {
        assert!(!UpdateUiState::new(UpdatePhase::Idle).shows_strip());
        assert!(!UpdateUiState::new(UpdatePhase::Checking).shows_strip());
        assert!(!UpdateUiState::new(UpdatePhase::UpToDate).shows_strip());
        assert!(UpdateUiState::new(UpdatePhase::Ready).shows_strip());
        assert!(UpdateUiState::new(UpdatePhase::Failed).shows_strip());
    }

    #[test]
    fn invalid_progress_is_clamped_for_display() {
        let state = UpdateUiState::new(UpdatePhase::Downloading).with_progress(Some(1.8));
        assert_eq!(state.progress_percent(), Some(100));
        assert_eq!(state.status_text(), "Downloading update (100%)");
    }

    #[test]
    fn a_build_that_cannot_update_itself_never_claims_the_notice() {
        let mut notice = StartupNotice::default();
        assert!(!notice.claim(false));
        // A build that gains actions later still has its notice unspent.
        assert!(notice.claim(true));
    }

    #[test]
    fn the_notice_is_claimed_once_per_run() {
        let mut notice = StartupNotice::default();
        assert!(notice.claim(true));
        assert!(!notice.claim(true));
    }

    #[gpui::test]
    fn callbacks_are_cloneable_and_runnable(cx: &mut TestAppContext) {
        let calls = Rc::new(Cell::new(0));
        let check_calls = calls.clone();
        let retry_calls = calls.clone();
        let restart_calls = calls.clone();
        let actions = UpdateActions::new(
            move |_| check_calls.set(check_calls.get() + 1),
            move |_| retry_calls.set(retry_calls.get() + 1),
            move |_| restart_calls.set(restart_calls.get() + 1),
        );
        let cloned = actions.clone();
        cx.update(|cx| {
            actions.run_check(cx);
            cloned.run_retry(cx);
            cloned.run_restart(cx);
        });
        assert_eq!(calls.get(), 3);
    }
}
