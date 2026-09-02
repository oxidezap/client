//! Where everything the daemon says goes out, and the four different
//! disciplines it says it under.
//!
//! Nothing here knows what the daemon *knows* — it holds no state, only the
//! ends of channels — which is what lets each discipline be argued for on its
//! own terms:
//!
//! * **[`Fanout::claim`] and the update channel** carry state. Frames are
//!   pre-serialized once for every reader, they are versioned, and a reader
//!   that falls behind is told to resynchronize, because a snapshot is what
//!   recovers them.
//! * **Signals** carry what is not state: a window request, a send that
//!   failed. They have no version and no snapshot holds them, so they must not
//!   travel on a channel a client stops reading while it resynchronizes.
//! * **Session events** are the account's whole traffic, for a front end that
//!   owns chats and messages rather than a summary of them. Opt-in, and
//!   preparing one is expensive enough that [`Fanout::session_events_wanted`]
//!   is asked first.
//! * **Video** is drop-newest-wins. Depth is the *problem* here: every frame
//!   held is latency between the person talking and the person watching.
//! * **The tray** is a `watch`, which only keeps the latest value and
//!   coalesces bursts on its own.
//!
//! Two of those channels are also *flow control*, and the answer is given
//! back from the act rather than peeked at beforehand: see [`Delivery`].

use std::sync::Arc;
use std::sync::Mutex;

use oxidezap_ipc::{DaemonEvent, DaemonMessage, StateVersion};
use tokio::sync::{broadcast, watch};

use super::store::TrayState;

/// How many frames a slow client may fall behind before its stream is
/// truncated.
///
/// Bounded on purpose: an unbounded queue would let one stalled client grow
/// the daemon's memory without limit. A client that overruns it gets
/// `Resync` and rebuilds from a snapshot, which is correct, just more
/// expensive than keeping up.
pub(super) const BROADCAST_CAPACITY: usize = 256;

/// How many pass-through frames may queue for one client.
///
/// Small, because these are user-initiated: a tray click, a send that failed.
/// A client far enough behind to overrun even this has bigger problems than a
/// missed window raise, and unlike state there is nothing to converge — a
/// dropped one is simply gone, which is why it is not worth buffering deeply.
const SIGNAL_CAPACITY: usize = 32;

/// How many session events a front end may fall behind by.
///
/// Deeper than the summary stream: one history load is a single event but a
/// burst of messages is many, and a front end that overruns has to rebuild
/// from a fresh load rather than from a cheap snapshot.
const SESSION_CAPACITY: usize = 1024;

/// A quarter of a second of video at the rate a call runs, and no more. This
/// is the one channel where depth is the *problem*: every frame held here is
/// latency between the person talking and the person watching, and a reader
/// that cannot keep up should be shown the newest frame rather than led
/// through the backlog.
pub(super) const VIDEO_CAPACITY: usize = 8;

/// Whether a frame had anybody to go to.
///
/// The answer to a publish rather than a question asked before one. Video is
/// produced by the session and consumed by whoever is drawing, and nothing
/// announces the last window going away — so the producer finds out by
/// offering a frame and being told nobody took it, which costs exactly the one
/// frame. Asked as a separate peek it was also a lie by the time it was acted
/// on: the reader could leave in between, and the frame was then dropped by a
/// second check inside the publish while the caller, having been told
/// otherwise, left the camera running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a frame nobody took is how a producer learns to stop producing"]
pub enum Delivery {
    /// Somebody was listening, so the frame went out.
    Taken,
    /// Nobody was listening. Producing more of these is work for no reader.
    Unwanted,
}

impl Delivery {
    /// Whether the producer should stop.
    #[must_use]
    pub fn is_unwanted(self) -> bool {
        self == Self::Unwanted
    }
}

/// The ends of every channel the daemon publishes on.
pub struct Fanout {
    /// Held from inside the state lock until a frame is on the channel, so
    /// versions leave in the order they were assigned. See [`Fanout::claim`].
    ordering: Mutex<()>,
    /// Pre-serialized frames. `Arc<str>` so fanning out to N clients costs N
    /// refcount bumps rather than N serializations.
    updates: broadcast::Sender<Arc<str>>,
    /// Frames that are not state, on their own channel.
    ///
    /// Separate from `updates` because a client that has been told to resync
    /// stops reading that one until its snapshot arrives, and a window
    /// request published in that window would simply be lost: it has no
    /// version, and no snapshot contains it. The tray's Open item doing
    /// nothing precisely while the front end is recovering is the failure
    /// this avoids.
    signals: broadcast::Sender<Arc<str>>,
    /// The session's own events, for front ends that asked for them.
    ///
    /// Its own channel rather than a flag on `updates`: a tray subscribes to
    /// summaries and would otherwise pay the serialization of every message in
    /// the account, and a front end subscribes to events and has no use for
    /// summaries it derives itself.
    sessions: broadcast::Sender<Arc<str>>,
    /// A live call's video, for front ends that draw it.
    ///
    /// A channel of its own because it obeys neither of the other two's rules.
    /// State converges from a snapshot and news must not be lost; a video
    /// frame is neither — the newest one is the only one worth having, and a
    /// client that falls behind is *right* to skip. Sharing `sessions` would
    /// turn a slow window into a `Resync` and throw its whole history away to
    /// recover a picture that had already moved on.
    video: broadcast::Sender<Arc<str>>,
    tray: watch::Sender<TrayState>,
    /// How many attached clients own a window.
    ///
    /// Not the same question as how many are subscribed: every client reads
    /// the signal channel, and only a front end can act on a window request.
    /// A TUI or a notifier attached to summaries would otherwise make
    /// [`crate::window::show`] believe there was a window to raise.
    windows: std::sync::atomic::AtomicUsize,
}

/// The right to publish next, taken while the version being published was
/// still being assigned.
///
/// Consuming it is what sends, so a writer cannot hold the order and forget to
/// use it, and cannot publish without having held it.
pub(super) struct Claim<'a> {
    fanout: &'a Fanout,
    /// Dropped with the claim, once everything this version owed is out.
    _order: std::sync::MutexGuard<'a, ()>,
}

impl Fanout {
    pub(super) fn new() -> Self {
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (signals, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (sessions, _) = broadcast::channel(SESSION_CAPACITY);
        let (video, _) = broadcast::channel(VIDEO_CAPACITY);
        let (tray, _) = watch::channel(TrayState {
            connected: false,
            unread: 0,
        });
        Self {
            ordering: Mutex::new(()),
            updates,
            signals,
            sessions,
            video,
            tray,
            windows: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Take the next place in the publication order.
    ///
    /// Called with the state lock still held, and released only once the
    /// version it belongs to has been published. The lock is recovered from
    /// poisoning rather than panicked on — unlike the state's, which guards
    /// a set of fields that must agree, this one protects nothing but the
    /// order of two sends, and a writer that panicked mid-send left no
    /// inconsistent value behind.
    pub(super) fn claim(&self) -> Claim<'_> {
        Claim {
            fanout: self,
            _order: self
                .ordering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    pub(super) fn subscribe_updates(&self) -> broadcast::Receiver<Arc<str>> {
        self.updates.subscribe()
    }

    pub(super) fn subscribe_signals(&self) -> broadcast::Receiver<Arc<str>> {
        self.signals.subscribe()
    }

    pub(super) fn subscribe_sessions(&self) -> broadcast::Receiver<Arc<str>> {
        self.sessions.subscribe()
    }

    pub(super) fn subscribe_video(&self) -> broadcast::Receiver<Arc<str>> {
        self.video.subscribe()
    }

    pub(super) fn watch_tray(&self) -> watch::Receiver<TrayState> {
        self.tray.subscribe()
    }

    /// Whether a state frame would reach anyone.
    ///
    /// With no clients the daemon still tracks state for the tray, but must
    /// not pay to format frames nobody reads.
    pub(super) fn updates_wanted(&self) -> bool {
        self.updates.receiver_count() > 0
    }

    /// Whether any front end is listening for session events.
    ///
    /// Asked before the work of preparing one: media has to be written to the
    /// cache before an event can be serialized, and with nobody attached that
    /// is a copy of every photo in the account for no reader. Unlike video,
    /// the caller cannot be answered by the act — what it is deciding is
    /// whether to *build* the thing it would publish.
    pub(super) fn session_events_wanted(&self) -> bool {
        self.sessions.receiver_count() > 0
    }

    /// Publish one video frame, and say whether anybody took it.
    ///
    /// Serialized only when there is a reader, which is the whole reason the
    /// answer is here: base64 and a JSON pass over every access unit of a call
    /// nobody is watching is real work for no reader — and a daemon holding a
    /// call with its window closed is the ordinary case.
    pub(super) fn publish_video(&self, frame: oxidezap_core::CallVideoFrame) -> Delivery {
        if self.video.receiver_count() == 0 {
            return Delivery::Unwanted;
        }
        match serde_json::to_string(&DaemonMessage::CallVideo(Box::new(frame))) {
            Ok(line) => {
                let _ = self.video.send(Arc::from(line.as_str()));
            }
            // Somebody is watching and this frame is the daemon's own fault.
            // Answering `Unwanted` would stop the camera over a bug in the
            // serializer, which is a black picture rather than a dropped one.
            Err(e) => log::error!("dropping an unserializable video frame: {e}"),
        }
        Delivery::Taken
    }

    /// Publish one session event, already serialized.
    pub(super) fn publish_session(&self, frame: String) {
        let _ = self.sessions.send(Arc::from(frame.as_str()));
    }

    /// Publish a frame that is not state.
    ///
    /// No version, because nothing changed: a window request is passed
    /// through to whoever owns a window, and a failed send is news about one
    /// message rather than a fact a snapshot could hold. Serialized only when
    /// someone is listening, like every other frame.
    pub(super) fn signal(&self, message: &DaemonMessage) {
        if self.signals.receiver_count() == 0 {
            return;
        }
        match serde_json::to_string(message) {
            // Err means every receiver dropped since the count above.
            Ok(line) => {
                let _ = self.signals.send(Arc::from(line.as_str()));
            }
            Err(e) => log::error!("dropping unserializable frame: {e}"),
        }
    }

    pub(super) fn attach_window(&self) {
        self.windows
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn detach_window(&self) {
        self.windows
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn windows_attached(&self) -> bool {
        self.windows.load(std::sync::atomic::Ordering::Relaxed) > 0
    }
}

impl Claim<'_> {
    /// Put one version's whole answer out, and give up the claim.
    ///
    /// The frame and the tray value together. The tray used to be sent after
    /// the claim was released, which made it the one thing about a version
    /// that could arrive out of order: two writers hand over the claim in
    /// version order, but the sends that followed it raced, and a `watch`
    /// keeps whichever arrived last. The icon then showed the older count
    /// until something unrelated moved.
    pub(super) fn publish(self, version: StateVersion, event: DaemonEvent, tray: TrayState) {
        // Serialized once, and only when someone is listening.
        if self.fanout.updates_wanted() {
            match serde_json::to_string(&DaemonMessage::Update { version, event }) {
                Ok(line) => {
                    // Err means every receiver dropped between the count above
                    // and here. Nothing to do: the state is already recorded.
                    let _ = self.fanout.updates.send(Arc::from(line.as_str()));
                }
                Err(e) => log::error!("dropping unserializable event: {e}"),
            }
        }

        // `send_if_modified` so an update that leaves the tray identical wakes
        // nothing: receipts and typing churn state constantly without changing
        // what the icon shows.
        self.fanout.tray.send_if_modified(|current| {
            if *current == tray {
                false
            } else {
                *current = tray;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_frame(call_id: &str) -> oxidezap_core::CallVideoFrame {
        oxidezap_core::CallVideoFrame::new(
            call_id.to_string(),
            oxidezap_core::VideoStream::Remote,
            vec![0, 0, 0, 1, 0x65],
            true,
            0,
        )
    }

    /// The producer's flow control is the answer to the publish, not a
    /// question asked of the channel beforehand. Nothing outside this file
    /// counts receivers.
    #[test]
    fn a_video_frame_says_whether_anybody_took_it() {
        let out = Fanout::new();
        assert_eq!(out.publish_video(video_frame("call")), Delivery::Unwanted);

        let reader = out.subscribe_video();
        assert_eq!(out.publish_video(video_frame("call")), Delivery::Taken);

        drop(reader);
        assert!(
            out.publish_video(video_frame("call")).is_unwanted(),
            "the last window leaving is noticed at the cost of one frame"
        );
    }

    /// One version, one publication: the frame and the tray value leave under
    /// the same claim, so a second writer cannot overtake either of them.
    #[tokio::test]
    async fn a_version_publishes_its_frame_and_its_tray_together() {
        let out = Fanout::new();
        let mut updates = out.subscribe_updates();
        let mut tray = out.watch_tray();

        out.claim().publish(
            StateVersion::INITIAL.next(),
            DaemonEvent::ChatRemoved {
                jid: "a@s.whatsapp.net".into(),
            },
            TrayState {
                connected: true,
                unread: 3,
            },
        );

        assert!(updates.try_recv().is_ok(), "the frame is out");
        assert_eq!(tray.borrow_and_update().unread, 3, "and so is the tray");
    }
}
