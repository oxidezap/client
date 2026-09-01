//! The queue a page drains.
//!
//! Unbounded, and nothing here ever waits: the thread that would wait for
//! room is the thread that makes it.
//!
//! The two ends are still two types. Not because this platform needs them
//! apart — it does not — but because everything above this is written once and
//! must not learn which platform it is on.

use super::Dropped;
use crate::session::FromDaemon;

/// The half the front end drains.
pub type Events = tokio::sync::mpsc::UnboundedReceiver<FromDaemon>;

/// The half the reader publishes on.
///
/// Not `Clone`, for the reason the desktop's is not: there is one reader.
pub struct ReaderSink(tokio::sync::mpsc::UnboundedSender<FromDaemon>);

/// The half everything on the UI executor publishes on.
#[derive(Clone)]
pub struct UiSink(tokio::sync::mpsc::UnboundedSender<FromDaemon>);

/// The pair, sized for this platform.
#[must_use]
pub fn channel() -> (ReaderSink, Events) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (ReaderSink(tx), rx)
}

impl ReaderSink {
    /// Publish. There is nothing here to wait for.
    ///
    /// Fails only when the front end has gone, which is the reader's signal
    /// to stop.
    pub fn send(&self, event: FromDaemon) -> Result<(), ()> {
        self.0.send(event).map_err(|_| ())
    }

    /// The end for everything that runs on the UI executor.
    pub fn ui(&self) -> UiSink {
        UiSink(self.0.clone())
    }
}

impl UiSink {
    /// Publish without ever waiting.
    ///
    /// The same call as [`ReaderSink::send`] here: the distinction the two
    /// types carry is the desktop's bounded queue, and their callers are
    /// shared. A page can only fail the one way.
    pub fn try_send(&self, event: FromDaemon) -> Result<(), Dropped> {
        self.0.send(event).map_err(|_| {
            log::debug!("nothing is listening for session events any more");
            Dropped::Gone
        })
    }
}
