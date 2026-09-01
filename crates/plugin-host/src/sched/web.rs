//! A task per plugin on the page's own loop, and real awaits on it.
//!
//! There is no thread to give a plugin and nothing here may block: blocking
//! the one agent a browser lends the page is what stops the page from
//! drawing. So the queue is an async one and the waits are `setTimeout`,
//! which is what makes a plugin between events cost a suspended task.
//!
//! What is lost against the desktop is naming: a browser task has no name,
//! and [`Task::join`] cannot wait — `spawn_local` hands a future to the loop
//! and forgets it. The host's shutdown does not depend on the wait, only on
//! the flag it raises and the sender it drops; see `Plugins::shutdown`.

use std::time::Duration;

use wacore::time::Instant;

use super::{TrySend, Wake};

/// `setTimeout`, as a future, from the crate that owns the browser's clock.
///
/// It used to be written out here, worker arm and drop guard and all, because
/// this host cannot depend on the session that had already written it. That
/// is what `oxidezap-platform` is for. Parking where no timer can be armed is
/// its behaviour as well as this one's, and for the same reason: every caller
/// here is a wait inside a loop, so returning at once turns one into a spin
/// that never yields and takes the tab with it.
pub use oxidezap_platform::sleep;

/// A bounded queue on the page's loop.
#[must_use]
pub fn channel<T>(depth: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = tokio::sync::mpsc::channel(depth);
    (Sender(tx), Receiver(rx))
}

/// The end the host offers jobs on.
pub struct Sender<T>(tokio::sync::mpsc::Sender<T>);

impl<T> Sender<T> {
    /// Queue `value`, or give it back.
    ///
    /// # Errors
    ///
    /// The queue is full, or the worker is gone.
    pub fn try_send(&self, value: T) -> Result<(), TrySend<T>> {
        use tokio::sync::mpsc::error::TrySendError;
        match self.0.try_send(value) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(value)) => Err(TrySend::Full(value)),
            Err(TrySendError::Closed(_)) => Err(TrySend::Closed),
        }
    }
}

/// The end the worker takes them from.
pub struct Receiver<T>(tokio::sync::mpsc::Receiver<T>);

impl<T> Receiver<T> {
    /// The next job, or whichever comes first: `deadline`, or the queue
    /// closing.
    pub async fn next_before(&mut self, deadline: Option<Instant>) -> Wake<T> {
        let Some(due) = deadline else {
            return match self.0.recv().await {
                Some(value) => Wake::Ready(value),
                None => Wake::Closed,
            };
        };
        let wait = due.saturating_duration_since(Instant::now());
        // `recv` is cancel-safe, which is what makes racing it against a
        // timer sound: the job that was not taken is still on the queue.
        futures_lite::future::or(
            async {
                match self.0.recv().await {
                    Some(value) => Wake::Ready(value),
                    None => Wake::Closed,
                }
            },
            async {
                sleep(wait).await;
                Wake::Elapsed
            },
        )
        .await
    }
}

/// Give the page's loop a turn.
///
/// A zero-length `setTimeout` rather than a bare yield, because what has to
/// run in the gap is the browser's own work — a frame, an input event — and
/// not merely another Rust task on the same tick.
pub async fn breathe() {
    sleep(Duration::ZERO).await;
}

/// Put a plugin's whole life on the page's loop.
///
/// # Errors
///
/// None here, and the signature keeps the `Result` because the interface is
/// one interface: a page has one loop and `spawn_local` finds it.
pub fn spawn(_name: &str, work: impl super::Work) -> std::io::Result<Task> {
    oxidezap_platform::spawn(work);
    Ok(Task)
}

/// A running plugin, which is a task the loop holds and nothing here does.
pub struct Task;

impl Task {
    /// There is nothing to wait for, and answering `true` says so.
    ///
    /// What ends a plugin on this platform is the same two things that end
    /// one on a thread — the stopping flag it reads between events, and its
    /// queue being dropped — and neither needs a join. What a page loses is
    /// the guarantee that the handler in flight has *returned* before the
    /// daemon goes on; a page's teardown is the tab closing, which takes the
    /// task with it either way.
    #[must_use]
    pub fn join(self) -> bool {
        true
    }
}
