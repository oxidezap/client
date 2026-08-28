//! The browser executor: the page's own event loop.
//!
//! There is no runtime to build and no thread to build one on. Everything
//! runs as a task on the loop the browser already turns, which is also why
//! nothing here is `Send`: a task spawned this way never leaves the agent
//! that spawned it, and the `web-sys` objects the session's transport holds
//! could not survive being moved anyway.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use super::{Cancelled, MaybeSend};

/// The page's loop, plus whether the session's own future is still on it.
pub struct Executor {
    /// Set when [`start`](Executor::start)'s future returns.
    ///
    /// A page has no thread to join, so this is the only honest thing
    /// [`join`](Executor::join) has to report.
    finished: Rc<Cell<bool>>,
}

impl Executor {
    /// Infallible, unlike the desktop half: there is nothing to build.
    ///
    /// The signature keeps the `Result` because the interface is one
    /// interface, and a caller that cannot fail is not worth a second shape.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            finished: Rc::new(Cell::new(false)),
        })
    }

    /// Put `future` on the page's loop.
    ///
    /// The name is the desktop's thread name and has nowhere to go here; a
    /// browser task has no name to take.
    pub fn start(
        &mut self,
        _name: &str,
        future: impl Future<Output = ()> + 'static,
    ) -> std::io::Result<()> {
        let finished = self.finished.clone();
        wasm_bindgen_futures::spawn_local(async move {
            future.await;
            finished.set(true);
        });
        Ok(())
    }

    /// A handle for spawning onto the page's loop from somewhere else later.
    #[allow(dead_code)]
    pub fn spawner(&self) -> Spawner {
        Spawner
    }

    /// Spawn a task on the page's loop.
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        let (tx, rx) = futures_channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            // Dropped rather than sent when nobody is waiting, which is what
            // turns a discarded `Task` into a `Cancelled` for anyone who is.
            let _ = tx.send(future.await);
        });
        Task(rx)
    }

    /// Whether the session's future has already stopped.
    ///
    /// It cannot wait, and the `timeout` is therefore ignored rather than
    /// honoured: blocking the page's one thread is what stops it from
    /// drawing, so a browser has no way to spend a timeout and still be a
    /// browser at the end of it. A page's real teardown is the tab going
    /// away, which takes the loop, the tasks and the memory with it — there
    /// is no equivalent of a process that outlives its session thread, which
    /// is the thing the desktop's wait exists to prevent.
    pub fn join(&mut self, _timeout: Duration) -> bool {
        self.finished.get()
    }
}

/// A handle that can spawn onto the page's loop later.
///
/// Unused on this target, because the one thing that needs a spawner rather
/// than the executor is a camera reporting that it died, and a page has no
/// camera. Present so the interface is one interface.
///
/// Carries nothing, because there is nothing to carry: a page has one loop
/// and `spawn_local` finds it from anywhere on the agent. See the desktop
/// half, where a runtime has to be named.
#[derive(Clone, Default)]
#[allow(dead_code)]
pub struct Spawner;

#[allow(dead_code)]
impl Spawner {
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        spawn(future)
    }
}

/// Spawn onto the loop this code is already running on.
///
/// The page has one, so this is [`Executor::spawn`] without the executor —
/// which is the whole difference from the desktop, where a task has to be
/// told which runtime it belongs to.
pub fn spawn<T: MaybeSend + 'static>(
    future: impl Future<Output = T> + MaybeSend + 'static,
) -> Task<T> {
    let (tx, rx) = futures_channel::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = tx.send(future.await);
    });
    Task(rx)
}

/// Run it here, because there is nowhere else.
///
/// A page has one thread and no pool to hand work to, so this is a call. That
/// is not a compromise for the one caller: what it runs is a bounded wait on
/// a session's loop finishing, and on this platform that wait does not block
/// — see [`Executor::join`], which cannot and does not try.
///
/// It stays `async` so that the callers read the same on both platforms, and
/// so this can become a real hand-off if a page ever gets somewhere to hand
/// work to.
pub async fn unblock<T: MaybeSend + 'static>(
    work: impl FnOnce() -> T + MaybeSend + 'static,
) -> Result<T, Cancelled> {
    Ok(work())
}

/// A spawned task's answer, carried by a channel the task sends on.
///
/// `spawn_local` hands back nothing to wait on, so the wait is built rather
/// than borrowed: the task sends its value and the receiver is the handle. A
/// task that panics drops its sender without sending, which the receiver
/// reads as [`Cancelled`] — the same answer the desktop gives for the same
/// event.
pub struct Task<T>(futures_channel::oneshot::Receiver<T>);

impl<T> Future for Task<T> {
    type Output = Result<T, Cancelled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.map_err(|_| Cancelled))
    }
}
