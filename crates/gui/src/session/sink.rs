//! Where the reader puts what it has read.
//!
//! One queue, two disciplines, because the two readers are not the same kind
//! of thing. A native front end reads on a thread of its own, so a full queue
//! is a reason to *stop reading* — the daemon then overruns its own bounded
//! broadcast and says `Resync`, which is the recovery this protocol already
//! has. A page has one thread, and it is the thread that drains this queue:
//! blocking on it would park the only thing that could empty it, so the queue
//! is unbounded there and the back pressure has nowhere to come from anyway.
//!
//! Both ends are the same two methods, so nothing above this knows which.

use super::FromDaemon;

/// How many session events may wait for a UI that is busy drawing.
///
/// Bounded on purpose. Unbounded, a stalled window keeps draining the socket
/// and buffering everything the account does, so the daemon sees a reader that
/// is keeping up and never truncates it — and this side grows without limit.
/// Bounded, the reader stops reading, the daemon's own bounded broadcast
/// overruns, and it says `Resync`. That is the recovery this protocol already
/// has; the point is to reach it rather than to hide from it.
#[cfg(not(target_family = "wasm"))]
const EVENT_QUEUE: usize = 512;

/// The half the front end drains.
#[cfg(not(target_family = "wasm"))]
pub type Events = tokio::sync::mpsc::Receiver<FromDaemon>;

/// The half the front end drains.
///
/// Unbounded, because the thread that would wait for room is the thread that
/// makes room.
#[cfg(target_family = "wasm")]
pub type Events = tokio::sync::mpsc::UnboundedReceiver<FromDaemon>;

/// The half the reader publishes on.
#[derive(Clone)]
pub struct EventSink(
    #[cfg(not(target_family = "wasm"))] tokio::sync::mpsc::Sender<FromDaemon>,
    #[cfg(target_family = "wasm")] tokio::sync::mpsc::UnboundedSender<FromDaemon>,
);

/// The pair, sized for this platform.
#[must_use]
pub fn channel() -> (EventSink, Events) {
    #[cfg(not(target_family = "wasm"))]
    {
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
        (EventSink(tx), rx)
    }
    #[cfg(target_family = "wasm")]
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (EventSink(tx), rx)
    }
}

impl EventSink {
    /// Publish, waiting for room where waiting is possible.
    ///
    /// Fails only when the front end has gone, which is the reader's signal
    /// to stop.
    pub fn send(&self, event: FromDaemon) -> Result<(), ()> {
        #[cfg(not(target_family = "wasm"))]
        {
            self.0.blocking_send(event).map_err(|_| ())
        }
        #[cfg(target_family = "wasm")]
        {
            self.0.send(event).map_err(|_| ())
        }
    }

    /// Publish without ever waiting.
    ///
    /// For the paths that run *on* the UI executor — a send that failed
    /// before it left this process, a status view that never went out. There,
    /// waiting for room would park the only thread that empties the queue,
    /// and a queue that full has bigger problems than one lost failure.
    pub fn try_send(&self, event: FromDaemon) {
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = self.0.try_send(event);
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = self.0.send(event);
        }
    }
}
