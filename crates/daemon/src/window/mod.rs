//! Making sure there is a window, from the side that does not own one — and
//! asking for it to go away again.
//!
//! The tray's Open and a client's `ShowWindow` both mean the same thing: the
//! user wants the interface up. Whoever already has a window raises it, and
//! that half is the same everywhere — it is a message on the signal channel,
//! and every attached front end reads it.
//!
//! What differs is what happens when *nobody* answers. A daemon beside a
//! desktop can start one, which is the mirror of the front end starting a
//! daemon it could not find. A page cannot: there is no second process to
//! launch, and the tab that would raise itself is the only window there is.
//! So the browser half stops after the message, which is not a stub — it is
//! the whole of what "make sure there is a window" can mean there.
//!
//! Hiding has no such half. It is only ever a message, because a window that
//! is not there needs nothing done to it — so [`hide`] and the [`toggle`]
//! the tray icon is clicked through are written once, here, and only
//! [`show`] is split.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

use oxidezap_ipc::DaemonMessage;

use crate::state::StateHub;

pub use platform::show;

/// Ask whoever owns a window to put it away.
///
/// A request and nothing more: the daemon has no window to close, and what
/// "away" means is the front end's — on a desktop the window is the process,
/// so this ends the same way closing it does, with the daemon holding the
/// account. Published to nobody it is harmless, which is the difference
/// between this and [`show`]: there is no second half to reach for.
pub fn hide(hub: &StateHub) {
    hub.signal(&DaemonMessage::HideWindow);
}

/// What a click on the tray icon means: put the window away if there is
/// one, bring one up if there is not.
///
/// Decided by who said they own a window (see [`StateHub::windows_attached`]),
/// not by whether a signal would reach anyone — every client reads the
/// signal channel, and a notifier watching summaries must not turn the click
/// into a hide that nothing acts on.
pub fn toggle(hub: &StateHub) {
    if hub.windows_attached() {
        hide(hub);
    } else {
        show(hub);
    }
}

// Native only: the web daemon's tests run in a browser, and these need
// `tokio::test`. Nothing here is platform-specific, so nothing is lost.
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    fn next_signal(
        signals: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<str>>,
    ) -> DaemonMessage {
        serde_json::from_str(&signals.try_recv().expect("a signal was published")).unwrap()
    }

    /// Hiding is a message, however many windows there are. There is no
    /// launch on the way back, so publishing it to nobody is fine.
    #[tokio::test]
    async fn hiding_asks_rather_than_acts() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();

        hide(&hub);

        assert_eq!(next_signal(&mut signals), DaemonMessage::HideWindow);
    }

    /// The bug this exists for: the icon had an Open and nothing that
    /// undid it. Clicked over a live window, it asks that window to go.
    #[tokio::test]
    async fn a_click_over_a_window_hides_it() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();
        let _window = hub.attach_window();

        toggle(&hub);

        assert_eq!(next_signal(&mut signals), DaemonMessage::HideWindow);
    }

    /// And clicked with nothing up, it is Open — the same request the menu
    /// item makes, so a front end that owns no window is asked for one.
    #[tokio::test]
    async fn a_click_with_no_window_opens_one() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();

        toggle(&hub);

        assert_eq!(next_signal(&mut signals), DaemonMessage::ShowWindow);
    }

    /// A subscriber that owns no window does not turn the click into a hide:
    /// a notifier reading summaries is exactly the client that would
    /// otherwise make the icon do nothing.
    #[tokio::test]
    async fn a_subscriber_without_a_window_does_not_flip_the_click() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();
        let _watcher = hub.subscribe_signals();

        toggle(&hub);

        assert_eq!(next_signal(&mut signals), DaemonMessage::ShowWindow);
    }
}
