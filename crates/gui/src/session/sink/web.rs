//! The queue a page drains.
//!
//! Unbounded, and neither method ever waits: the thread that would wait for
//! room is the thread that makes it.

use crate::session::FromDaemon;

/// The half the front end drains.
pub type Events = tokio::sync::mpsc::UnboundedReceiver<FromDaemon>;

/// The half the reader publishes on.
#[derive(Clone)]
pub struct EventSink(tokio::sync::mpsc::UnboundedSender<FromDaemon>);

/// The pair, sized for this platform.
#[must_use]
pub fn channel() -> (EventSink, Events) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (EventSink(tx), rx)
}

impl EventSink {
    /// Publish. There is nothing here to wait for.
    ///
    /// Fails only when the front end has gone, which is the reader's signal
    /// to stop.
    pub fn send(&self, event: FromDaemon) -> Result<(), ()> {
        self.0.send(event).map_err(|_| ())
    }

    /// Publish without ever waiting.
    ///
    /// For the paths that run *on* the UI executor — a send that failed
    /// before it left this process, a status view that never went out. The
    /// same call as [`send`](Self::send) here: the distinction the two names
    /// carry is the desktop's bounded queue, and their callers are shared.
    pub fn try_send(&self, event: FromDaemon) {
        let _ = self.0.send(event);
    }
}
