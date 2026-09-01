//! Where a task runs, and how it waits.
//!
//! Four things differ between a desktop and a page, and only four: what a
//! future is spawned onto, what a wait is armed on, whether a task may be
//! moved between threads, and — as a consequence of the first two — how a
//! deadline is raced. Everything else above this crate is written once.
//!
//! This is deliberately the *bottom* of the workspace. The session's `exec`
//! module used to be the seam, and the tree's own rule said to go through it
//! rather than naming a clock or a pool — but `plugin-host` sits beside the
//! session rather than above it and cannot depend on it, so it grew its own
//! copy of the same `setTimeout`, and the window grew a third. Three copies
//! of one timer is what an unenforceable rule costs. A crate underneath all
//! of them is at least *reachable* from every one of them, which the session
//! was not — nothing yet stops a fourth copy, and the tree still calls
//! `spawn_local` directly in a couple of dozen places; a `disallowed-methods`
//! entry is what would, the way the shared-`ArrayBufferView` ban does.
//!
//! It is four functions and a marker trait, and that is the whole intended
//! size of it. A capability that has a platform split of its own — audio,
//! video, storage, a transport — keeps that split where the capability is;
//! what is here is only what *every* crate needs and no crate can supply for
//! itself.

use std::future::Future;
use std::time::Duration;

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub use platform::{sleep, spawn, try_sleep};

/// [`Send`], where the platform's executor asks for it.
///
/// A work-stealing runtime moves a task between threads, so everything it is
/// given must be `Send`. A browser's event loop moves nothing anywhere, and
/// requiring it there would rule out every `web-sys` object — none of which
/// is `Send`, and none of which needs to be. The same trick the library plays
/// with `MaybeSendSync`, for the same reason.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> MaybeSend for T {}

/// See the desktop half: on a page the bound is empty.
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSend for T {}

/// Whichever finishes first: the work, or the wait. `None` when the wait won.
///
/// A race rather than `tokio::time::timeout`, so that it is written once: the
/// timer a race needs is [`sleep`], which each platform already answers, and
/// on a desktop that is the same wheel `timeout` would have registered on.
/// A page has no wheel to put a deadline in at all — there is only
/// `setTimeout` and whichever of the two futures returns first.
pub async fn with_timeout<T>(work: impl Future<Output = T>, limit: Duration) -> Option<T> {
    futures_lite::future::or(async { Some(work.await) }, async {
        sleep(limit).await;
        None
    })
    .await
}
