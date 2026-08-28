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

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use super::{Cancelled, MaybeSend};

/// The page's loop, plus whether the session's own future is still on it.
pub struct Executor {
    /// Set when [`start`](Executor::start)'s future returns.
    finished: Rc<Cell<bool>>,
    /// Raised at the same moment, for whoever is waiting.
    ///
    /// The flag alone cannot be waited on, and waiting is the whole point:
    /// the one caller decides whether an account's store may be deleted, and
    /// a page that answered "still closing" because it had not yielded yet
    /// would refuse every wipe there is.
    done: Rc<tokio::sync::Notify>,
}

impl Executor {
    /// Infallible, unlike the desktop half: there is nothing to build.
    ///
    /// The signature keeps the `Result` because the interface is one
    /// interface, and a caller that cannot fail is not worth a second shape.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            finished: Rc::new(Cell::new(false)),
            done: Rc::new(tokio::sync::Notify::new()),
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
        let done = self.done.clone();
        wasm_bindgen_futures::spawn_local(async move {
            future.await;
            finished.set(true);
            done.notify_waiters();
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

    /// Wait for the session's future to end, up to `timeout`.
    ///
    /// A page cannot block, so this waits the way a page waits: the loop
    /// keeps turning and this task is woken when the session's own future
    /// returns, or when a `setTimeout` says the grace is spent.
    ///
    /// It used to answer without waiting at all, on the grounds that a tab
    /// has no thread to join — which read as "already finished" to the one
    /// caller that matters. That caller decides whether an account's store
    /// may be deleted, and it is told to refuse when the session is still
    /// closing: "clear data and pair again" would have refused every time,
    /// left the dead credentials in place, and reopened them on the retry.
    pub async fn join(&mut self, timeout: Duration) -> bool {
        if self.finished.get() {
            return true;
        }
        // Registered before the wait, so a session that ends between the
        // check above and here is not missed: `notify_waiters` wakes whoever
        // is already waiting and nobody else.
        let ended = self.done.notified();
        futures_lite::future::or(ended, sleep(timeout)).await;
        self.finished.get()
    }
}

/// `setTimeout`, as a future.
///
/// Resolves immediately where no timer can be armed — a worker with no
/// `window` — rather than never, because a future that never completes holds
/// whatever is awaiting it for the life of the page.
pub async fn sleep(duration: Duration) {
    /// Disarms the timer when the sleep is dropped.
    ///
    /// A `setTimeout` left armed fires into a `Closure` that has already been
    /// freed, which is a wasm-bindgen panic rather than a missed wakeup — and
    /// this is raced against something that routinely wins.
    struct Timer {
        handle: i32,
        _fire: Closure<dyn FnMut()>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(self.handle);
            }
        }
    }

    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    let Some(window) = web_sys::window() else {
        return;
    };
    let mut tx = Some(tx);
    let fire = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    });
    let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        fire.as_ref().unchecked_ref(),
        i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
    ) else {
        return;
    };
    let _timer = Timer {
        handle,
        _fire: fire,
    };
    let _ = rx.await;
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

/// Drop it here, because here is the only place there is.
///
/// The desktop half has to release a Tokio runtime somewhere blocking is
/// allowed. A page owns no runtime and has nowhere else, so this is a drop
/// with a signature that matches.
pub async fn let_go<T: MaybeSend + 'static>(value: T) {
    drop(value);
}

/// Whichever finishes first: the work, or the wait. `None` when the wait won.
///
/// Raced rather than driven by a timer wheel, because the timer here is the
/// browser's own `setTimeout` and there is no wheel to put it in.
pub async fn with_timeout<T>(
    work: impl Future<Output = T>,
    limit: std::time::Duration,
) -> Option<T> {
    futures_lite::future::or(async { Some(work.await) }, async {
        sleep(limit).await;
        None
    })
    .await
}
