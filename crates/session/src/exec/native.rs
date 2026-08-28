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
    pub fn join(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.worker.take() else {
            return true;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(timeout).is_ok()
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
