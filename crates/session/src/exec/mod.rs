//! Where the session's work runs.
//!
//! A desktop session owns a multi-threaded Tokio runtime on a thread of its
//! own, because it is a library talking to a socket and a SQLite pool while
//! the process it lives in is doing something else. A page owns none of that
//! and cannot: `wasm32-unknown-unknown` has no threads to build a runtime on
//! and no way to block the one thread the browser lends it, since blocking it
//! is what stops the page from drawing.
//!
//! So this is the same shape as [`crate::net`]: one interface the session
//! calls, two implementations behind it, and no `cfg` anywhere above. The
//! session says *spawn this* and *run this until it stops*; which executor
//! hears it is not its business.
//!
//! The bound is the whole difference. A future handed to a work-stealing
//! runtime can be moved between threads and has to be [`Send`]; one handed to
//! the page's event loop never leaves the agent that owns it and must not be
//! required to, since the browser objects it holds are not `Send` and no
//! amount of wrapping makes them so. [`MaybeSend`] is that difference, named
//! once — the same trick the library plays with `MaybeSendSync`, for the same
//! reason.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

/// Where the session waits.
///
/// [`sleep`] and [`with_timeout`] are here rather than reached for from
/// `tokio::time`, which is where a timeout would ordinarily come from and
/// which does not work on a page: tokio's clock is `std::time::Instant::now`
/// with no platform under it, so the first `sleep` or `timeout` on
/// `wasm32-unknown-unknown` traps with "time not implemented on this
/// platform". That the crate *compiles* for the target says nothing about the
/// timer running on it — a distinction only running the page can make, and it
/// made it.
pub use platform::{Executor, Task, let_go, sleep, spawn, unblock, with_timeout};

/// [`Send`], where the platform's executor asks for it.
///
/// A work-stealing runtime moves a task between threads, so everything it is
/// given must be `Send`. A browser's event loop moves nothing anywhere, and
/// requiring it there would rule out every `web-sys` object — none of which
/// is `Send`, and none of which needs to be.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> MaybeSend for T {}

/// See the desktop half: on a page the bound is empty.
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSend for T {}

/// The task did not finish, and nothing is coming.
///
/// One name for two platforms' ways of losing a task — a panic or an abort on
/// a runtime, a dropped sender on a page — because a caller cannot act on the
/// difference. Every caller here answers it the same way: say the session
/// stopped before the answer arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the task did not finish")
    }
}

impl std::error::Error for Cancelled {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain case, and the reason the type is a `Result` at all: what came
    /// back is what the future returned, not a handle to go and ask again.
    #[tokio::test]
    async fn a_task_yields_what_its_future_returned() {
        assert_eq!(spawn(async { 7 }).await, Ok(7));
    }

    /// A lost task is one answer, whichever way it was lost.
    ///
    /// Every caller in the session says the same thing about it — the session
    /// stopped before the answer arrived — so the two platforms' ways of
    /// losing one (a panic here, a dropped sender on a page) must not arrive
    /// as two different things to distinguish between.
    #[tokio::test]
    async fn a_task_that_dies_reads_as_cancelled() {
        let lost = spawn(async {
            panic!("the task went away");
        })
        .await;
        assert_eq!(lost, Err(Cancelled));
    }
}
