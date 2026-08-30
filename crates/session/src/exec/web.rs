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

    /// Spawn a task on the page's loop, owned by this session.
    ///
    /// Owned because everything reaching this is the session's own work — the
    /// call backends are its only callers — and [`join`](Self::join) has to
    /// wait for it. See [`spawn_owned`].
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        spawn_owned(future)
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
        // One deadline over both waits below, not one each: the caller is
        // asking how long it is willing to wait in total before deciding the
        // session is wedged.
        let deadline = sleep(timeout);
        futures_lite::future::or(
            async {
                if !self.finished.get() {
                    // Registered before the wait, so a session that ends
                    // between the check above and here is not missed:
                    // `notify_waiters` wakes whoever is already waiting and
                    // nobody else.
                    let ended = self.done.notified();
                    ended.await;
                }
                // And then its children. `run_client` returning is not the
                // end of its work here — the tasks it spawned are held by the
                // page's loop, not by anything that just went out of scope,
                // and one of them holding the store is what turns a wipe into
                // a deletion under a live connection. They stop when the
                // session tells them to; this is where that is waited for.
                while Outstanding::any() {
                    let drained = DRAINED.with(|drained| Rc::clone(drained));
                    let waited = drained.notified();
                    if !Outstanding::any() {
                        break;
                    }
                    waited.await;
                }
            },
            deadline,
        )
        .await;
        self.finished.get() && !Outstanding::any()
    }
}

/// Whichever global this agent has a `setTimeout` on.
///
/// A window in the page and a `WorkerGlobalScope` in a worker. Both carry the
/// same two methods and neither inherits from the other, so the choice is
/// made once, here.
enum Timers {
    Window(web_sys::Window),
    Worker(web_sys::WorkerGlobalScope),
}

thread_local! {
    /// The agent's global, resolved once.
    ///
    /// `web_sys::window()` is an `instanceof` across the wasm/JS boundary and
    /// the worker fallback is a second one, which is a strange price to pay
    /// per `setTimeout` on a path this hot: the library yields once per item
    /// it processes, so a history sync arms one of these per message.
    static TIMERS: Option<Timers> = Timers::here();
}

/// Do something with this agent's timers, if it has any.
fn with_timers<T>(f: impl FnOnce(&Timers) -> T) -> Option<T> {
    // `try_with` rather than `with`: a `Timer` can be dropped while thread
    // locals are being destroyed, and a panic there is a panic in a
    // destructor.
    TIMERS
        .try_with(|timers| timers.as_ref().map(f))
        .ok()
        .flatten()
}

impl Timers {
    fn here() -> Option<Self> {
        if let Some(window) = web_sys::window() {
            return Some(Self::Window(window));
        }
        js_sys::global()
            .dyn_into::<web_sys::WorkerGlobalScope>()
            .ok()
            .map(Self::Worker)
    }

    fn arm(&self, fire: &Closure<dyn FnMut()>, millis: i32) -> Result<i32, wasm_bindgen::JsValue> {
        match self {
            Self::Window(window) => window.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            ),
            Self::Worker(worker) => worker.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            ),
        }
    }

    fn disarm(&self, handle: i32) {
        match self {
            Self::Window(window) => window.clear_timeout_with_handle(handle),
            Self::Worker(worker) => worker.clear_timeout_with_handle(handle),
        }
    }
}

/// `setTimeout`, as a future.
///
/// Parks forever where no timer can be armed, which is what the window's own
/// clock does and for the same reason: every caller here is a loop that waits
/// — a reconnect backoff, the QR rotation, a keepalive — so returning at once
/// turns one into a spin that never yields and takes the tab with it.
/// Stopping the loop is the honest outcome, and the log is what says so.
pub async fn sleep(duration: Duration) {
    /// Disarms the timer when the sleep is dropped.
    ///
    /// A `setTimeout` left armed fires into a `Closure` that has already been
    /// freed, which is a wasm-bindgen panic rather than a missed wakeup — and
    /// this is raced against something that routinely wins.
    struct Timer {
        handle: i32,
        /// Raised by the callback below. A timer that has already fired has
        /// nothing to cancel, and `clearTimeout` is a call across the
        /// boundary either way — paid on every sleep, every retry and every
        /// yield the library makes, which on this target is one per item.
        fired: Rc<Cell<bool>>,
        _fire: Closure<dyn FnMut()>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            if self.fired.get() {
                return;
            }
            with_timers(|timers| timers.disarm(self.handle));
        }
    }

    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    let mut tx = Some(tx);
    let fired = Rc::new(Cell::new(false));
    let raise = Rc::clone(&fired);
    let fire = Closure::<dyn FnMut()>::new(move || {
        raise.set(true);
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    });
    let armed = with_timers(|timers| {
        timers.arm(
            &fire,
            i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
        )
    })
    .and_then(|handle| handle.ok())
    .map(|handle| Timer {
        handle,
        fired,
        _fire: fire,
    });
    let Some(_timer) = armed else {
        log::error!("this agent has no timer; the loop that was waiting on one stops here");
        std::future::pending::<()>().await;
        return;
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
    /// Spawn onto the page's loop, owned by the session. See [`spawn_owned`].
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        spawn_owned(future)
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

/// Spawn a task the session owns, and that [`Executor::join`] waits for.
///
/// The distinction is not tidiness. [`spawn`] is also what the *daemon* uses
/// — its bridge and its connections are tasks on this same loop — and the
/// bridge is the thing that calls `join`, from inside itself, to decide
/// whether an account's database may be deleted. Counting every task on the
/// agent would count the caller, so the wait could never end and "clear data
/// and pair again" would refuse every time. Counted here is what the session
/// started and what the session can stop.
pub fn spawn_owned<T: MaybeSend + 'static>(
    future: impl Future<Output = T> + MaybeSend + 'static,
) -> Task<T> {
    let (tx, rx) = futures_channel::oneshot::channel();
    let counted = Outstanding::enter();
    wasm_bindgen_futures::spawn_local(async move {
        let _outstanding = counted;
        let _ = tx.send(future.await);
    });
    Task(rx)
}

// How many spawned tasks are still running on this agent.
//
// A desktop session ends by dropping the runtime it was built on, and every
// task on it goes at the same moment — so "the session has finished" and "its
// work has finished" are one fact there. Here they are two: `spawn_local`
// hands a future to the page's loop and forgets it, so `run_client` can
// return while tasks it started are still awaiting things.
//
// That difference reaches exactly one caller, and the caller is the one that
// matters most: the answer to "may this account's database be deleted now".
// A task still holding the store when the file goes is a wipe racing a live
// connection.
thread_local! {
    static OUTSTANDING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Woken when the count reaches zero.
    static DRAINED: Rc<tokio::sync::Notify> = Rc::new(tokio::sync::Notify::new());
}

/// One spawned task's presence in the count, released when it is dropped.
///
/// A guard rather than a decrement at the end of the future, because a future
/// that panics or is dropped part-way still has to leave the count — and a
/// count that only ever goes up would make every later teardown wait out its
/// whole grace period.
struct Outstanding;

impl Outstanding {
    /// Join the count, for as long as the returned guard lives.
    fn enter() -> Self {
        OUTSTANDING.with(|count| count.set(count.get() + 1));
        Self
    }

    /// Whether anything spawned here is still running.
    fn any() -> bool {
        OUTSTANDING.with(std::cell::Cell::get) > 0
    }
}

impl Drop for Outstanding {
    /// Leave the count, and wake [`Executor::join`] if this was the last one.
    ///
    /// The whole mechanism is this drop: a task that panics, or is dropped
    /// part-way, leaves the count exactly as one that ran to completion does.
    fn drop(&mut self) {
        let left = OUTSTANDING.with(|count| {
            let left = count.get().saturating_sub(1);
            count.set(left);
            left
        });
        if left == 0 {
            DRAINED.with(|drained| drained.notify_waiters());
        }
    }
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
