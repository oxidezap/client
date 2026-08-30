//! A thread per plugin, and blocking waits on it.
//!
//! Everything here blocks, deliberately. A wasm call is synchronous and a
//! `Store` is not shareable, so a plugin owns a thread and there is nothing
//! else on it to be polite to; the `async` shape exists so the worker loop is
//! written once, and `block_on` here is what turns it back into the thread
//! this platform has always given a plugin.

use std::sync::mpsc::{Receiver as StdReceiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

use wacore::time::Instant;

use super::{TrySend, Wake};

/// A bounded queue: the same `sync_channel` a plugin worker has always had.
#[must_use]
pub fn channel<T>(depth: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(depth);
    (Sender(tx), Receiver(rx))
}

/// The end the host offers jobs on.
pub struct Sender<T>(SyncSender<T>);

impl<T> Sender<T> {
    /// Queue `value`, or give it back.
    ///
    /// # Errors
    ///
    /// The queue is full, or the worker is gone.
    pub fn try_send(&self, value: T) -> Result<(), TrySend<T>> {
        match self.0.try_send(value) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(value)) => Err(TrySend::Full(value)),
            Err(TrySendError::Disconnected(_)) => Err(TrySend::Closed),
        }
    }
}

/// The end the worker takes them from.
pub struct Receiver<T>(StdReceiver<T>);

impl<T> Receiver<T> {
    /// The next job, or whichever comes first: `deadline`, or the queue
    /// closing.
    ///
    /// Blocking, and it does not yield: the future this belongs to is driven
    /// by [`spawn`]'s `block_on` on a thread of its own.
    pub async fn next_before(&mut self, deadline: Option<Instant>) -> Wake<T> {
        match deadline {
            Some(due) => {
                let wait = due.saturating_duration_since(Instant::now());
                match self.0.recv_timeout(wait) {
                    Ok(value) => Wake::Ready(value),
                    Err(RecvTimeoutError::Timeout) => Wake::Elapsed,
                    Err(RecvTimeoutError::Disconnected) => Wake::Closed,
                }
            }
            None => match self.0.recv() {
                Ok(value) => Wake::Ready(value),
                Err(_) => Wake::Closed,
            },
        }
    }
}

/// Nothing to yield to: the loader owns this thread.
pub async fn breathe() {}

/// Hold this thread. See the module note: there is nothing else on it.
pub async fn sleep(duration: Duration) {
    std::thread::sleep(duration);
}

/// Put a plugin's whole life on a thread of its own.
///
/// # Errors
///
/// The thread could not be started, which the caller answers by publishing
/// the plugin as stopped with the reason beside it.
pub fn spawn(name: &str, work: impl super::Work) -> std::io::Result<Task> {
    std::thread::Builder::new()
        .name(name.to_owned())
        // `futures_lite`'s parker rather than a Tokio runtime, and that is
        // load-bearing: a plugin's command goes to the daemon through
        // `blocking_send`, which panics when it is called from inside a
        // runtime's context. The worker's future never awaits anything that
        // is not one of this module's own blocking calls, so a parker is the
        // whole executor it needs.
        .spawn(move || futures_lite::future::block_on(work))
        .map(Task)
}

/// A running plugin's thread.
pub struct Task(std::thread::JoinHandle<()>);

impl Task {
    /// Wait for the handler it is in the middle of, and answer whether it
    /// ended cleanly.
    #[must_use]
    pub fn join(self) -> bool {
        self.0.join().is_ok()
    }
}
