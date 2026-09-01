//! The queue a front end with a reader thread drains.
//!
//! Bounded, and the reader filling it can afford to wait: it is a thread of
//! its own, so a queue with no room is a reason to stop reading rather than a
//! way to deadlock.

use crate::session::FromDaemon;

/// How many session events may wait for a UI that is busy drawing.
///
/// Bounded on purpose. Unbounded, a stalled window keeps draining the socket
/// and buffering everything the account does, so the daemon sees a reader that
/// is keeping up and never truncates it — and this side grows without limit.
/// Bounded, the reader stops reading, the daemon's own bounded broadcast
/// overruns, and it says `Resync`. That is the recovery this protocol already
/// has; the point is to reach it rather than to hide from it.
const EVENT_QUEUE: usize = 512;

/// The half the front end drains.
pub type Events = tokio::sync::mpsc::Receiver<FromDaemon>;

/// The half the reader publishes on.
#[derive(Clone)]
pub struct EventSink(tokio::sync::mpsc::Sender<FromDaemon>);

/// The pair, sized for this platform.
#[must_use]
pub fn channel() -> (EventSink, Events) {
    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    (EventSink(tx), rx)
}

impl EventSink {
    /// Publish, waiting for room where waiting is possible.
    ///
    /// Fails only when the front end has gone, which is the reader's signal
    /// to stop.
    pub fn send(&self, event: FromDaemon) -> Result<(), ()> {
        self.0.blocking_send(event).map_err(|_| ())
    }

    /// Publish without ever waiting.
    ///
    /// For the paths that run *on* the UI executor — a send that failed
    /// before it left this process, a status view that never went out. There,
    /// waiting for room would park the only thread that empties the queue,
    /// and a queue that full has bigger problems than one lost failure.
    pub fn try_send(&self, event: FromDaemon) {
        let _ = self.0.try_send(event);
    }
}
