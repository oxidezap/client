//! Waiting, where there is no reactor to wait on.
//!
//! The window waits for a lot of small things — a recording tick, a call's
//! second hand, a typing monitor, a pairing countdown, a frame of video — and
//! all of them were `smol::Timer::after`. That timer is backed by `async-io`,
//! which is an epoll loop: a page has none, and pulling one in drags `rustix`
//! and `errno` behind it, neither of which builds for a target whose OS is
//! "unknown".
//!
//! A browser's own timer is `setTimeout`, and bridging it is a `Closure`, a
//! drop guard and a `WorkerGlobalScope` fallback — which this module used to
//! carry, minus the fallback, alongside two other copies elsewhere in the
//! tree. It is `oxidezap_platform::sleep` now, and the window is the third
//! caller of one timer rather than the author of a third one.
//!
//! What stays here is the *desktop* half, and that is the reason this module
//! is not simply the shared crate's own `sleep` on both sides: the shared one
//! waits on a Tokio wheel, and the window has no runtime to carry a wheel. A
//! front end has not owned one since the daemon took the session over.

use std::time::Duration;

/// Wait, and come back.
///
/// Not a precise clock on either side: a browser clamps a background tab's
/// timers and a desktop timer is subject to the executor. Every caller here
/// is a repaint or a poll, which is exactly the kind of thing that may be
/// late.
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_family = "wasm"))]
    {
        smol::Timer::after(duration).await;
    }
    #[cfg(target_family = "wasm")]
    {
        oxidezap_platform::sleep(duration).await;
    }
}

/// Whichever finishes first: the work, or the wait.
///
/// `None` when the wait won. Written here rather than at the call site
/// because the two halves come from different places on the web — the future
/// is ours and the timer is the browser's — and racing them is the only thing
/// a caller wants. Over the [`sleep`] above rather than the shared crate's
/// `with_timeout`, for the reason the module header gives: that one is a race
/// against a Tokio timer, and this side of the window has no runtime.
pub async fn with_timeout<T>(work: impl Future<Output = T>, limit: Duration) -> Option<T> {
    let timeout = sleep(limit);
    futures_lite::future::or(async { Some(work.await) }, async {
        timeout.await;
        None
    })
    .await
}
