//! The desktop executor: a Tokio runtime on a thread of the session's own.
//!
//! The thread is the point. `bot.run()` reconnects internally and never
//! returns, so the session's loop cannot run on a caller's thread without
//! taking it forever — and the runtime has to outlive every task it spawned,
//! which is why it is an `Arc` the worker carries rather than a local.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use super::{Cancelled, MaybeSend};

/// A multi-threaded Tokio runtime, and the thread it is driven on.
pub struct Executor {
    runtime: Arc<tokio::runtime::Runtime>,
    /// The session thread, kept joinable.
    ///
    /// [`Executor::join`] only waits; stopping is the session's own business
    /// and it has its own notification for that. Without a handle to wait on,
    /// a process that exits right after asking can die mid-teardown, because
    /// Rust does not wait for threads when `main` returns.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Executor {
    /// Build the runtime.
    ///
    /// Fallible because building one can fail on resource exhaustion, and a
    /// session that cannot start should route to an error screen rather than
    /// panic the thread that asked for it.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?,
            ),
            worker: None,
        })
    }

    /// Drive `future` to completion on a thread of this executor's own.
    ///
    /// Letting `block_on` return is what drops the runtime and the SQLite
    /// pool with it, so the future given here is the session's whole life.
    pub fn start(
        &mut self,
        name: &str,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let runtime = self.runtime.clone();
        self.worker = Some(std::thread::Builder::new().name(name.to_string()).spawn(
            move || {
                runtime.block_on(future);
            },
        )?);
        Ok(())
    }

    /// A handle for spawning onto this executor from somewhere else later.
    pub fn spawner(&self) -> Spawner {
        Spawner(self.runtime.handle().clone())
    }

    /// Spawn a task on the runtime.
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        Task(self.runtime.spawn(future))
    }

    /// Wait for [`start`](Self::start)'s future to finish, and say whether it
    /// did within `timeout`.
    ///
    /// Bounded, so a wedged session delays exit rather than preventing it —
    /// and bounded *here* rather than by the caller, because `JoinHandle` has
    /// no timed join. A second thread does the untimed one and reports
    /// through a channel this side can give up on.
    ///
    /// `async`, and the wait happens on a blocking thread: a runtime worker
    /// parked in a join is a worker not driving anything else, including the
    /// session that is trying to finish.
    pub async fn join(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.worker.take() else {
            return true;
        };
        oxidezap_session_join(handle, timeout).await
    }
}

/// The blocking half of [`Executor::join`], off the runtime.
async fn oxidezap_session_join(handle: std::thread::JoinHandle<()>, timeout: Duration) -> bool {
    unblock(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(timeout).is_ok()
    })
    .await
    .unwrap_or(false)
}

/// A handle that can spawn onto this executor later, from anywhere.
///
/// For the callbacks: a camera's pump reports that it died from whichever
/// thread noticed, which is not a thread the runtime knows about, so
/// [`spawn`] would find no runtime to attach to. A `Handle` is what carries
/// one across — cheap to clone, `Send + Sync`, and valid for as long as the
/// runtime it names.
#[derive(Clone)]
pub struct Spawner(tokio::runtime::Handle);

impl Spawner {
    pub fn spawn<T: MaybeSend + 'static>(
        &self,
        future: impl Future<Output = T> + MaybeSend + 'static,
    ) -> Task<T> {
        Task(self.0.spawn(future))
    }
}

/// Spawn onto the runtime this code is already running on.
///
/// For the session's own loop, which is inside `block_on` and has no
/// [`Executor`] to hand: a task spawned from there belongs to the runtime
/// that is driving it, and `tokio::spawn` is how one says so.
pub fn spawn<T: MaybeSend + 'static>(
    future: impl Future<Output = T> + MaybeSend + 'static,
) -> Task<T> {
    Task(tokio::spawn(future))
}

/// Run something that blocks, somewhere blocking is allowed.
///
/// A join with a timeout is the caller here, and a runtime thread is not
/// allowed to sit in one: everything else that runtime is driving would stop
/// with it.
pub async fn unblock<T: MaybeSend + 'static>(
    work: impl FnOnce() -> T + MaybeSend + 'static,
) -> Result<T, Cancelled> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Cancelled)
}

/// A spawned task's answer.
///
/// Awaiting it yields what the future returned, or [`Cancelled`] if the task
/// panicked or the runtime went away underneath it.
pub struct Task<T>(tokio::task::JoinHandle<T>);

impl<T> Future for Task<T> {
    type Output = Result<T, Cancelled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.map_err(|_| Cancelled))
    }
}

/// Drop something where dropping it is allowed.
///
/// The executor owns a Tokio runtime, and tokio refuses to drop one inside an
/// async context — "Cannot drop a runtime in a context where blocking is not
/// allowed" — so whatever owns one has to be released off the runtime.
pub async fn let_go<T: MaybeSend + 'static>(value: T) {
    let _ = unblock(move || drop(value)).await;
}

/// Wait, on the runtime that is already here.
///
/// The web half is `setTimeout`; this one is the timer wheel a Tokio runtime
/// already carries. Both exist so that nothing above [`super`] reaches for
/// `tokio::time` directly — that reaches a clock a browser does not have.
pub async fn sleep(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}

/// Whichever finishes first: the work, or the wait. `None` when the wait won.
pub async fn with_timeout<T>(
    work: impl Future<Output = T>,
    limit: std::time::Duration,
) -> Option<T> {
    tokio::time::timeout(limit, work).await.ok()
}
