//! Where a plugin's worker runs, and how it waits.
//!
//! A plugin gets a thread and blocks on a queue, which is the whole of what
//! kept plugins off the web: `wasm32-unknown-unknown` has neither to give.
//! The answer is the same shape as the session's own `exec` module — one
//! interface the worker calls, two implementations behind it, and no `cfg` in
//! the worker itself.
//!
//! The two are not the same mechanism wearing different names, and the
//! difference is worth stating: on a desktop every function here *blocks*,
//! because the future it belongs to is driven by a `block_on` on a thread
//! that has nothing else to do — the loop below is written `async` so that
//! one loop serves both platforms, not because a desktop plugin yields to
//! anything. In a page nothing may block, so the same calls are real awaits
//! on the loop the browser turns, and a plugin between events costs the page
//! a suspended task rather than a thread.

use std::future::Future;
use std::time::Duration;

#[cfg_attr(target_family = "wasm", path = "sched/web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "sched/native.rs")]
mod platform;

pub use platform::{Receiver, Sender, Task, breathe, channel, sleep, spawn};

/// [`Send`], where the platform's executor asks for it.
///
/// A worker thread is handed its `Runtime` across a thread boundary; a page's
/// loop moves nothing anywhere and would rule out every browser object the
/// storage behind a plugin holds. The same trick the session plays, for the
/// same reason.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> MaybeSend for T {}

/// See the desktop half: on a page the bound is empty.
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSend for T {}

/// What a full or closed queue gives back.
///
/// The value comes back with `Full`, because the host's whole queue rule is
/// about what to do with a job that would not fit — stop the plugin, or
/// refuse the press — and neither can be decided without it.
pub enum TrySend<T> {
    Full(T),
    Closed,
}

/// What ended a worker's wait.
pub enum Wake<T> {
    /// Something to run.
    Ready(T),
    /// The deadline came first: a timer is due, or a setting is owed a write.
    Elapsed,
    /// Every sender is gone, which is how a worker is told to stop.
    Closed,
}

/// A future that is polled to completion on whatever this platform runs on.
///
/// Named once so the two spawns agree about what they take.
pub trait Work: Future<Output = ()> + MaybeSend + 'static {}
impl<T: Future<Output = ()> + MaybeSend + 'static> Work for T {}

/// The longest one uninterruptible slice of a throttled plugin's wait.
///
/// A plugin being held back is still one the daemon has to be able to join,
/// so a debt of minutes is slept in slices and the flag is read between them.
pub const SLICE: Duration = Duration::from_millis(50);
