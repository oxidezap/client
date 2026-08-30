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
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use super::{TrySend, Wake};

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

/// Whichever global this agent has a `setTimeout` on.
///
/// A window in the page and a `WorkerGlobalScope` in a worker. Both carry the
/// same two methods and neither inherits from the other, so the choice is
/// made once — and made at all, because a plugin worker is where this is
/// eventually meant to run.
enum Timers {
    Window(web_sys::Window),
    Worker(web_sys::WorkerGlobalScope),
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
/// Parks forever where no timer can be armed, which is the same answer the
/// session's own clock gives: every caller here is a wait inside a loop, so
/// returning at once turns one into a spin that never yields and takes the
/// tab with it.
pub async fn sleep(duration: Duration) {
    /// Disarms the timer when the sleep is dropped — a `setTimeout` left
    /// armed fires into a freed `Closure`, which is a panic rather than a
    /// missed wakeup, and this is raced against something that wins often.
    struct Timer {
        timers: Timers,
        handle: i32,
        _fire: Closure<dyn FnMut()>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            self.timers.disarm(self.handle);
        }
    }

    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    let mut tx = Some(tx);
    let fire = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    });
    let armed = Timers::here().and_then(|timers| {
        let handle = timers
            .arm(
                &fire,
                i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
            )
            .ok()?;
        Some(Timer {
            timers,
            handle,
            _fire: fire,
        })
    });
    let Some(_timer) = armed else {
        log::error!("this agent has no timer; the plugin that was waiting on one stops here");
        std::future::pending::<()>().await;
        return;
    };
    let _ = rx.await;
}

/// Put a plugin's whole life on the page's loop.
///
/// # Errors
///
/// None here, and the signature keeps the `Result` because the interface is
/// one interface: a page has one loop and `spawn_local` finds it.
pub fn spawn(_name: &str, work: impl super::Work) -> std::io::Result<Task> {
    wasm_bindgen_futures::spawn_local(work);
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
