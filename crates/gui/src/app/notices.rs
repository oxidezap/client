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
//! The stack is a view of its own, floating over whatever screen is up — for
//! the reason the call card lives at the root: a failure is not the
//! conversation's, and one raised while Settings is open still has to be
//! readable. It draws itself rather than being drawn by the root, which is
//! what makes the sweeper's tick the notices' own business: the clock taking
//! a line down is not news to the chat list.
//!
//! It takes no keyboard at any point, which is the other half of that. A
//! transient surface that takes focus has to give it back, and the one that
//! neither offers an action nor accepts a key has nothing to give back — so
//! the whole stack is a click target and nothing more.

use std::time::Duration;

use gpui::{App, Context, Entity, IntoElement, Render, Task, WeakEntity, Window};

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

/// The stack, and the one timer that expires it.
///
/// An entity rather than three fields on the app, and a view rather than a
/// slice the root reads: a notice going up, being dismissed or lapsing is a
/// change to this and to nothing else, and the type of the context every
/// method here takes is what says so — none of them can reach a
/// `Context<WhatsAppApp>`, so none of them can mark the window's other state
/// as having moved.
pub struct Notices {
    /// Newest last, which is the order they are drawn in.
    shown: Vec<Notice>,
    /// Never reused, so a dismissal cannot land on a later notice.
    next_id: u64,
    /// Expires them. Alive only while something is up.
    sweeper: Option<Task<()>>,
}

impl Notices {
    pub(super) fn new() -> Self {
        Self {
            shown: Vec::new(),
            next_id: 0,
            sweeper: None,
        }
    }

    /// Say one sentence to whoever is looking.
    ///
    /// The text is shown verbatim, so it is written for a reader rather than
    /// assembled from an error chain: a caller that has a `Display` full of
    /// context should say the short thing here and log the long one.
    pub fn raise(&mut self, text: impl Into<String>, tone: Tone, cx: &mut Context<Self>) {
        self.push(
            text.into(),
            tone,
            wacore::time::now_utc()
                + chrono::Duration::from_std(LIFETIME).unwrap_or(chrono::Duration::seconds(6)),
        );
        self.sweep(cx);
        cx.notify();
    }

    /// Put one on the stack, under the cap.
    ///
    /// Apart from [`Self::raise`] because it is the half that has no window
    /// in it: the id it hands out and the line it drops are decisions, and
    /// the timer and the repaint around them are not.
    fn push(&mut self, text: String, tone: Tone, expires_at: chrono::DateTime<chrono::Utc>) {
        self.next_id = self.next_id.wrapping_add(1);
        self.shown.push(Notice {
            id: self.next_id,
            text,
            tone,
            expires_at,
        });
        // The newest are the ones worth reading, and they are at the end.
        let overflow = self.shown.len().saturating_sub(MAX_SHOWN);
        self.shown.drain(..overflow);
    }

    /// Take one down because it was dismissed.
    pub fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.take(id);
        cx.notify();
    }

    /// Take one down, without saying so to anybody.
    fn take(&mut self, id: u64) {
        self.shown.retain(|notice| notice.id != id);
    }

    /// Everything on screen, gone, and the timer with it.
    ///
    /// Called by an account reset rather than by the clock: "that recording
    /// could not be sent" is about an account that has gone, shown to whoever
    /// pairs next, and a reset is a departure rather than a clear. No notify,
    /// because the reset is a change to the whole window and the app's own
    /// notify covers this along with everything else it dropped.
    pub(super) fn forget(&mut self) {
        self.shown.clear();
        self.sweeper = None;
    }

    /// Keep one sweeper running for as long as anything is up.
    ///
    /// One task rather than a timer per notice: they all expire against the
    /// same clock, and a task per notice would be a task per burst of the
    /// failures that arrive in bursts. It ends itself when the last one goes,
    /// so an idle app holds no timer at all.
    fn sweep(&mut self, cx: &mut Context<Self>) {
        if self.sweeper.is_some() {
            return;
        }
        self.sweeper = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(SWEEP).await;
                let more = entity.update(cx, |notices, cx| {
                    if notices.expire(wacore::time::now_utc()) {
                        cx.notify();
                    }
                    !notices.shown.is_empty()
                });
                match more {
                    Ok(true) => continue,
                    // Nothing left to expire, or the view is gone.
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |notices, _| notices.sweeper = None);
        }));
    }

    /// Drop everything already due at `now`, saying whether anything went.
    ///
    /// Against a clock rather than a countdown per notice, so a sweep that
    /// runs late — a tab that was in the background, a thread that was
    /// busy — takes everything it should have taken rather than one.
    fn expire(&mut self, now: chrono::DateTime<chrono::Utc>) -> bool {
        let before = self.shown.len();
        self.shown.retain(|notice| notice.expires_at > now);
        self.shown.len() != before
    }
}

impl Render for Notices {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let me = cx.entity();
        crate::components::notice::render_notices(
            &self.shown,
            move |id, cx| {
                me.update(cx, |notices, cx| notices.dismiss(id, cx));
            },
            cx,
        )
    }
}

impl super::WhatsAppApp {
    /// Say one sentence to whoever is looking. See [`Notices::raise`].
    ///
    /// Kept on the app because the callers are all over it and a failure they
    /// want said is not a reason for them to learn where the stack lives.
    /// Takes `&mut App` rather than the app's own context on purpose: nothing
    /// about the app moved, so nothing here needs the power to say it did.
    pub fn notify_user(&mut self, text: impl Into<String>, tone: Tone, cx: &mut App) {
        self.notices
            .update(cx, |notices, cx| notices.raise(text, tone, cx));
    }

    /// The stack, for the root to hang over everything else it drew.
    pub(super) fn notices(&self) -> &Entity<Notices> {
        &self.notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp")
    }

    /// A [`Notices`] with no window behind it, raised through the same call
    /// the app makes: everything below is about the stack, and an entity's
    /// own state machine is reachable without a window.
    fn stack(expiries: &[i64]) -> Notices {
        let mut notices = Notices::new();
        for (offset, secs) in expiries.iter().enumerate() {
            notices.push(format!("notice {offset}"), Tone::Problem, at(*secs));
        }
        notices
    }

    /// The cap keeps the newest, because a burst shares a cause and the last
    /// one is the one still true.
    #[test]
    fn the_stack_drops_its_oldest_first() {
        let notices = stack(&[100, 100, 100, 100, 100]);

        assert_eq!(notices.shown.len(), MAX_SHOWN);
        assert_eq!(
            notices.shown.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    /// A stack under the cap is left alone: `saturating_sub` is what makes
    /// the drain a no-op rather than a panic on an empty range.
    #[test]
    fn a_short_stack_is_untouched() {
        let notices = stack(&[100, 100]);

        assert_eq!(notices.shown.len(), 2);
    }

    /// Expiry is against the clock rather than a countdown per notice, so a
    /// sweep that runs late takes everything it should have taken.
    #[test]
    fn a_late_sweep_takes_everything_already_due() {
        let mut notices = stack(&[50, 99, 150]);

        assert!(notices.expire(at(100)), "two of them were due");
        assert_eq!(
            notices.shown.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![3]
        );
    }

    /// The sweep says whether anything went, and that answer is the only
    /// thing that repaints: a tick that took nothing must not redraw the
    /// stack, or an idle notice would cost a frame a second.
    #[test]
    fn a_sweep_that_takes_nothing_says_so() {
        let mut notices = stack(&[150, 200]);

        assert!(!notices.expire(at(100)));
        assert_eq!(notices.shown.len(), 2);
    }

    /// Ids are never reused, so a dismissal that arrives after its notice
    /// lapsed cannot take down the one that replaced it.
    #[test]
    fn an_id_is_never_handed_out_twice() {
        let mut notices = stack(&[100, 100]);
        let first = notices.shown[0].id;

        notices.take(first);
        notices.push("later".to_string(), Tone::Problem, at(100));

        assert!(
            notices.shown.iter().all(|n| n.id != first),
            "the id that was dismissed does not come back"
        );
        assert_eq!(notices.shown.len(), 2, "and the one beside it stayed up");
    }

    /// An account reset is a departure, not a clear: the sentences describe
    /// an account that has gone, and the timer counting them down has
    /// nothing left to count.
    #[test]
    fn a_reset_takes_the_stack_and_its_timer() {
        let mut notices = stack(&[100, 100]);

        notices.forget();

        assert!(notices.shown.is_empty());
        assert!(notices.sweeper.is_none());
    }
}
