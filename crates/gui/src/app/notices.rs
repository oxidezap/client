//! Saying that something small went wrong, without ending anything.
//!
//! The app had exactly one visible error state, and it is
//! [`AppState::Error`](crate::AppState::Error): it leaves the connected view,
//! drops the conversation and schedules a reconnect. That is the right answer
//! for an outage and a catastrophic one for a save that did not start, so
//! every failure too small to justify it reached the log and nowhere else.
//!
//! A notice is the other end of that scale. It says one sentence, it expires
//! on its own, and it changes nothing: no screen leaves, no state advances,
//! nothing is retried. What it costs the person who asked is a glance, which
//! is the whole point — the alternative on the web is a first tap that
//! silently does nothing and a second one that works.
//!
//! The root draws them, for the reason the call card lives there: a failure
//! is not the conversation's, and one raised while Settings is open still has
//! to be readable.

use std::time::Duration;

use gpui::{Context, WeakEntity};

use super::WhatsAppApp;

/// How long a notice stays up.
///
/// Long enough to read a sentence twice, since the reader was not expecting
/// to have to read anything.
const LIFETIME: Duration = Duration::from_secs(6);

/// How often the sweeper looks.
///
/// A second is under the eye's tolerance for "it went away when it said it
/// would" and cheap enough that the loop costs nothing while it runs.
const SWEEP: Duration = Duration::from_secs(1);

/// How many are drawn at once.
///
/// A stack taller than this is not information, it is a wall — and the
/// failures that arrive in bursts are the ones with a common cause, so the
/// oldest are the least useful.
const MAX_SHOWN: usize = 3;

/// What a notice is about, which is all that separates them.
///
/// Deliberately two and not five. A notice cannot offer an action or carry a
/// title, so anything finer would be a colour with no meaning attached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Something the person asked for did not happen.
    Problem,
    /// Something happened that they did not ask about. Nothing raises one
    /// yet — `notice.rs` draws it, and the first caller takes this attribute
    /// off. Named rather than left to a crate-wide `allow(dead_code)`, which
    /// is what this crate stopped doing: one variant with a reason on it says
    /// what a blanket attribute hides.
    #[allow(dead_code)]
    Info,
}

/// One line, and when it stops being drawn.
pub struct Notice {
    /// Identifies it for dismissal. Never reused.
    pub id: u64,
    pub text: String,
    pub tone: Tone,
    /// Compared against the clock by the sweeper, so a notice raised while
    /// the tab was in the background does not outlive the others.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl WhatsAppApp {
    /// Say one sentence to whoever is looking.
    ///
    /// The text is shown verbatim, so it is written for a reader rather than
    /// assembled from an error chain: a caller that has a `Display` full of
    /// context should say the short thing here and log the long one.
    pub fn notify_user(&mut self, text: impl Into<String>, tone: Tone, cx: &mut Context<Self>) {
        self.next_notice_id = self.next_notice_id.wrapping_add(1);
        self.notices.push(Notice {
            id: self.next_notice_id,
            text: text.into(),
            tone,
            expires_at: wacore::time::now_utc()
                + chrono::Duration::from_std(LIFETIME).unwrap_or(chrono::Duration::seconds(6)),
        });
        // The newest are the ones worth reading, and they are at the end.
        let overflow = self.notices.len().saturating_sub(MAX_SHOWN);
        self.notices.drain(..overflow);
        self.sweep_notices(cx);
        cx.notify();
    }

    /// Take one down because it was dismissed.
    pub fn dismiss_notice(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.retain(|notice| notice.id != id);
        cx.notify();
    }

    /// Keep one sweeper running for as long as anything is up.
    ///
    /// One task rather than a timer per notice: they all expire against the
    /// same clock, and a task per notice would be a task per burst of the
    /// failures that arrive in bursts. It ends itself when the last one goes,
    /// so an idle app holds no timer at all.
    fn sweep_notices(&mut self, cx: &mut Context<Self>) {
        if self.notice_task.is_some() {
            return;
        }
        self.notice_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(SWEEP).await;
                let more = entity.update(cx, |app, cx| {
                    let now = wacore::time::now_utc();
                    let before = app.notices.len();
                    app.notices.retain(|notice| notice.expires_at > now);
                    if app.notices.len() != before {
                        cx.notify();
                    }
                    !app.notices.is_empty()
                });
                match more {
                    Ok(true) => continue,
                    // Nothing left to expire, or the view is gone.
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.notice_task = None);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(id: u64, secs: i64) -> Notice {
        Notice {
            id,
            text: format!("notice {id}"),
            tone: Tone::Problem,
            expires_at: chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp"),
        }
    }

    /// The cap keeps the newest, because a burst shares a cause and the last
    /// one is the one still true.
    #[test]
    fn the_stack_drops_its_oldest_first() {
        let mut notices: Vec<Notice> = (1..=5).map(|id| notice(id, 100)).collect();
        let overflow = notices.len().saturating_sub(MAX_SHOWN);
        notices.drain(..overflow);

        assert_eq!(notices.len(), MAX_SHOWN);
        assert_eq!(
            notices.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    /// A stack under the cap is left alone: `saturating_sub` is what makes
    /// the drain a no-op rather than a panic on an empty range.
    #[test]
    fn a_short_stack_is_untouched() {
        let mut notices: Vec<Notice> = (1..=2).map(|id| notice(id, 100)).collect();
        let overflow = notices.len().saturating_sub(MAX_SHOWN);
        notices.drain(..overflow);

        assert_eq!(notices.len(), 2);
    }

    /// Expiry is against the clock rather than a countdown per notice, so a
    /// sweep that runs late takes everything it should have taken.
    #[test]
    fn a_late_sweep_takes_everything_already_due() {
        let now = chrono::DateTime::from_timestamp(100, 0).expect("valid timestamp");
        let mut notices = vec![notice(1, 50), notice(2, 99), notice(3, 150)];
        notices.retain(|n| n.expires_at > now);

        assert_eq!(notices.iter().map(|n| n.id).collect::<Vec<_>>(), vec![3]);
    }
}
