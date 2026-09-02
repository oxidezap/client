//! Getting out of a failed connection.
//!
//! The error screen offers three ways forward, and this is what backs them:
//! retry now, wait for the automatic retry, or stop waiting and read what is
//! already on this device.
//!
//! The three things that answer for it — when the retry fires, the timer
//! counting it down, and whether the technical detail is unfolded — are an
//! entity of their own rather than fields on the app. Unlike the notices,
//! this one does not draw itself: the error screen *is* the window while it
//! is up, so the countdown's tick has to reach the root, and it does that
//! through the app handle it was armed with rather than by being read out of
//! the app's own state. Which is the point of the split even here — the
//! countdown says exactly which entity it needs, and it is not this one.

use gpui::{App, Context, Task, WeakEntity};
use oxidezap_core::{Fault, Recovery};

use super::*;

/// How long to wait before retrying by itself.
///
/// Long enough not to hammer a server that is refusing, short enough that
/// someone watching the screen sees it happen rather than giving up first.
const RETRY_AFTER_SECS: u64 = 15;

/// How often the countdown redraws, which is also how precise it can be.
const COUNTDOWN_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// What the error screen is waiting on, and what it is showing.
pub(super) struct Recovering {
    /// When the automatic retry fires, while the error screen is up.
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Counts it down. Only alive while the error screen is.
    countdown: Option<Task<()>>,
    /// Whether the error screen's technical detail is unfolded.
    detail_open: bool,
}

impl Recovering {
    pub(super) fn new() -> Self {
        Self {
            retry_at: None,
            countdown: None,
            detail_open: false,
        }
    }

    /// Seconds until the automatic retry, or `None` when none is scheduled.
    pub(super) fn countdown_secs(&self) -> Option<u64> {
        self.retry_at.map(|at| {
            let now = wacore::time::now_utc();
            (at - now).num_seconds().max(0) as u64
        })
    }

    pub(super) fn detail_open(&self) -> bool {
        self.detail_open
    }

    pub(super) fn toggle_detail(&mut self) {
        self.detail_open = !self.detail_open;
    }

    /// Fold the detail back, because the fault it was opened against is not
    /// the one on screen any more.
    ///
    /// A reader who unfolded an outage's stack trace, pressed Retry and got a
    /// different failure was shown the new sentence with the old fault's
    /// detail already open under it — which reads as detail *of* the new one.
    /// The fold is a question about a specific fault, so it is asked again
    /// with each.
    pub(super) fn close_detail(&mut self) {
        self.detail_open = false;
    }

    /// Stop waiting, and stop counting.
    ///
    /// The timer is dropped rather than left to notice, which is what
    /// dropping a [`Task`] means: nothing on screen is counting any more, so
    /// nothing should be waking up to say so. It would have stopped itself on
    /// its next tick — the app is no longer on the error screen — and this
    /// only saves the second in between, but it is also the one place that
    /// can say "this promise is over" without waiting for a clock.
    pub(super) fn stop(&mut self) {
        self.retry_at = None;
        self.countdown = None;
    }

    /// Arm the automatic retry and the countdown that shows it.
    ///
    /// Takes the app it is retrying *for*, because both things it does at the
    /// end are the app's: the retry itself, and the repaint of the label —
    /// the error screen is drawn by the root, so there is no smaller thing to
    /// mark. Weak, so a window that has gone takes the timer with it.
    pub(super) fn arm(&mut self, app: WeakEntity<WhatsAppApp>, cx: &mut Context<Self>) {
        if self.retry_at.is_some() {
            return;
        }
        self.retry_at =
            Some(wacore::time::now_utc() + chrono::Duration::seconds(RETRY_AFTER_SECS as i64));

        self.countdown = Some(cx.spawn(async move |me: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(COUNTDOWN_TICK).await;
                // Left the error screen: the retry is moot and the timer
                // should not fire into a connected app.
                let Ok(still_failed) =
                    app.update(cx, |app, _| matches!(app.app_state, AppState::Error(_)))
                else {
                    break;
                };
                if !still_failed {
                    let _ = me.update(cx, |recovering, _| recovering.retry_at = None);
                    break;
                }
                let Ok(due) = me.update(cx, |recovering, _| {
                    matches!(recovering.countdown_secs(), Some(0) | None)
                }) else {
                    break;
                };
                if due {
                    let _ = me.update(cx, |recovering, _| recovering.retry_at = None);
                    let _ = app.update(cx, |app, cx| app.retry_connection(cx));
                    break;
                }
                // Only the label changed, but that label is the whole point
                // of the countdown, and the root is what draws it.
                if app.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
            let _ = me.update(cx, |recovering, _| recovering.countdown = None);
        }));
    }
}

impl WhatsAppApp {
    /// Seconds until the automatic retry, or `None` when none is scheduled.
    pub fn retry_countdown(&self, cx: &App) -> Option<u64> {
        self.recovery.read(cx).countdown_secs()
    }

    /// Whether the error screen's technical detail is unfolded.
    pub(super) fn error_detail_open(&self, cx: &App) -> bool {
        self.recovery.read(cx).detail_open()
    }

    /// This connection is over, and this is what it was.
    ///
    /// One place, because the screen's promise depends on which: an outage is
    /// retried and a countdown means something; a window that fell behind is
    /// reattaching and there is nothing to count; a version mismatch will
    /// fail the same way forever, so arming a retry is a promise this cannot
    /// keep.
    pub(super) fn connection_ended(&mut self, fault: Fault, cx: &mut Context<Self>) {
        self.leave_connected_view(cx);
        let recovery = fault.recovery;
        // A new fault is a new sentence, and the fold under it belongs to the
        // one it replaces. See `Recovering::close_detail`.
        self.recovery
            .update(cx, |recovering, _| recovering.close_detail());
        self.app_state = AppState::Error(fault);
        match recovery {
            // Now, because the screen says so. A window that fell behind is
            // one the daemon is right there for, and arming the outage's
            // countdown left it sitting under a body that claimed it was
            // already attaching.
            Recovery::Now => self.retry_connection(cx),
            Recovery::AfterAWait => self.schedule_retry(cx),
            Recovery::Nothing => {}
        }
        cx.notify();
    }

    pub fn toggle_error_detail(&mut self, cx: &mut Context<Self>) {
        self.recovery
            .update(cx, |recovering, _| recovering.toggle_detail());
        // The root draws the error screen, so the root is what has to redraw:
        // the fold is the app's frame however small the state behind it is.
        cx.notify();
    }

    /// Stop retrying and use what is already here.
    ///
    /// History is local, so a failed connection does not make the app
    /// useless — it makes it read-only. Saying so is what stops the error
    /// screen from being a dead end. [`AppState::Offline`], not `Connected`:
    /// claiming the connection was restored is what let the composer accept
    /// a message nothing could carry.
    pub fn work_offline(&mut self, cx: &mut Context<Self>) {
        info!("Working offline; local history stays readable");
        self.recovery.update(cx, |recovering, _| recovering.stop());
        self.app_state = AppState::Offline;
        cx.notify();
    }

    /// Arm the automatic retry and the countdown that shows it.
    pub(super) fn schedule_retry(&mut self, cx: &mut Context<Self>) {
        let app = cx.entity().downgrade();
        self.recovery
            .update(cx, |recovering, cx| recovering.arm(app, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The countdown is derived from a deadline rather than decremented, so a
    /// tick that runs late lands on the right number instead of the number of
    /// ticks that happened to run.
    #[test]
    fn the_countdown_is_read_off_the_deadline() {
        let mut recovering = Recovering::new();
        assert_eq!(
            recovering.countdown_secs(),
            None,
            "nothing armed, nothing to count"
        );

        recovering.retry_at = Some(wacore::time::now_utc() + chrono::Duration::seconds(9));
        assert!(matches!(recovering.countdown_secs(), Some(8 | 9)));
    }

    /// A deadline already past reads as zero rather than as a negative
    /// number, which is what makes "fire when it reaches zero" a rule the
    /// timer can apply however late it wakes.
    #[test]
    fn a_deadline_already_past_reads_as_due() {
        let mut recovering = Recovering::new();
        recovering.retry_at = Some(wacore::time::now_utc() - chrono::Duration::seconds(30));

        assert_eq!(recovering.countdown_secs(), Some(0));
    }

    /// Working offline is a decision to stop waiting, so both halves of the
    /// wait go: the deadline the screen was counting to, and the timer
    /// counting it.
    #[test]
    fn working_offline_stops_the_wait() {
        let mut recovering = Recovering::new();
        recovering.retry_at = Some(wacore::time::now_utc() + chrono::Duration::seconds(15));

        recovering.stop();

        assert_eq!(recovering.countdown_secs(), None);
        assert!(recovering.countdown.is_none());
    }

    /// The fold is the reader's, and it survives everything except a new
    /// fault — which is what [`Recovering::close_detail`] answers for.
    #[test]
    fn the_detail_fold_is_a_toggle() {
        let mut recovering = Recovering::new();
        assert!(
            !recovering.detail_open(),
            "a fault reads as its sentence first"
        );

        recovering.toggle_detail();
        assert!(recovering.detail_open());
        recovering.toggle_detail();
        assert!(!recovering.detail_open());
    }

    /// A different failure is a different sentence, and the detail unfolded
    /// against the one before it would read as this one's.
    #[test]
    fn a_new_fault_folds_the_detail_back() {
        let mut recovering = Recovering::new();
        recovering.toggle_detail();

        recovering.close_detail();

        assert!(!recovering.detail_open());
        // And it is not a toggle: two faults in a row leave it folded.
        recovering.close_detail();
        assert!(!recovering.detail_open());
    }
}
