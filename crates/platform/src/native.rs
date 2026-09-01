//! The desktop half: the runtime that is already running, and its wheel.
//!
//! Nothing here builds an executor or a reactor. Every caller on this side is
//! already inside a Tokio runtime — the session's own, which `exec` builds
//! and drives — so spawning is `tokio::spawn` and waiting is the timer wheel
//! that runtime already carries.

use std::future::Future;
use std::time::Duration;

use crate::MaybeSend;

/// Hand `future` to the runtime this code is already running on.
///
/// Detached: the writer queue and the page's equivalent are loops that end
/// when their channel closes, so there is nothing a caller would hold and no
/// cancellation anybody would ask for. Whoever needs the answer back builds a
/// handle of its own around this — see `oxidezap_session::exec`.
pub fn spawn(future: impl Future<Output = ()> + MaybeSend + 'static) {
    tokio::spawn(future);
}

/// Wait, on the runtime that is already here.
///
/// The point of routing every wait through this is what it is *not*: nothing
/// above this crate reaches for `tokio::time` directly, because that reaches
/// a clock a browser does not have — and reaches it at run time, on a target
/// where the same call compiles perfectly.
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Always waits, and always says so. See the web half, where a page can be
/// left with no clock to arm.
pub async fn try_sleep(duration: Duration) -> bool {
    sleep(duration).await;
    true
}
