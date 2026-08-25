//! Getting out of a failed connection.
//!
//! The error screen offers three ways forward, and this is what backs them:
//! retry now, wait for the automatic retry, or stop waiting and read what is
//! already on this device.

use super::*;

/// How long to wait before retrying by itself.
///
/// Long enough not to hammer a server that is refusing, short enough that
/// someone watching the screen sees it happen rather than giving up first.
const RETRY_AFTER_SECS: u64 = 15;

impl WhatsAppApp {
    /// Seconds until the automatic retry, or `None` when none is scheduled.
    pub fn retry_countdown(&self) -> Option<u64> {
        self.retry_at.map(|at| {
            let now = whatsapp_rust::wacore::time::now_utc();
            (at - now).num_seconds().max(0) as u64
        })
    }

    pub fn is_error_detail_open(&self) -> bool {
        self.error_detail_open
    }

    pub fn toggle_error_detail(&mut self, cx: &mut Context<Self>) {
        self.error_detail_open = !self.error_detail_open;
        cx.notify();
    }

    /// Stop retrying and use what is already here.
    ///
    /// History is local, so a failed connection does not make the app
    /// useless — it makes it read-only. Saying so is what stops the error
    /// screen from being a dead end.
    pub fn work_offline(&mut self, cx: &mut Context<Self>) {
        info!("Working offline; local history stays readable");
        self.retry_at = None;
        self.app_state = AppState::Connected;
        cx.notify();
    }

    /// Arm the automatic retry and the countdown that shows it.
    pub(super) fn schedule_retry(&mut self, cx: &mut Context<Self>) {
        if self.retry_at.is_some() {
            return;
        }
        self.retry_at = Some(
            whatsapp_rust::wacore::time::now_utc()
                + chrono::Duration::seconds(RETRY_AFTER_SECS as i64),
        );

        self.retry_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                smol::Timer::after(std::time::Duration::from_secs(1)).await;
                let fire = entity.update(cx, |app, cx| {
                    // Left the error screen: the retry is moot and the timer
                    // should not fire into a connected app.
                    if !matches!(app.app_state, AppState::Error(_)) {
                        app.retry_at = None;
                        return None;
                    }
                    match app.retry_countdown() {
                        Some(0) | None => Some(true),
                        Some(_) => {
                            // Only the label changed, but that label is the
                            // whole point of the countdown.
                            cx.notify();
                            Some(false)
                        }
                    }
                });
                match fire {
                    Ok(Some(true)) => {
                        let _ = entity.update(cx, |app, cx| {
                            app.retry_at = None;
                            app.retry_connection(cx);
                        });
                        break;
                    }
                    Ok(Some(false)) => continue,
                    // Cancelled, or the view is gone.
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.retry_task = None);
        }));
    }
}
